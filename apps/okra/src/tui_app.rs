//! `okra tui` — the M4 pager (MASTER-PLAN §3 #55): a ratatui block
//! scrollback over REAL turns. The turn engine lives on a dedicated
//! worker thread (the Agent owns its kernel handle; the render loop only
//! ever touches the `Scrollback` model), events cross as serialized
//! LoopEvents, and scrolling freezes against streaming appends
//! (crates/tui/src/pager.rs owns those semantics, unit-tested).
//!
//! Keys: Enter sends · PgUp/PgDn/j/k/G scroll · Ctrl-C stops a running
//! turn, quits when idle.

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line as UiLine;
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use okra_tui::pager::{LineKind, Scrollback};

enum ToWorker {
    Send(String),
    Stop,
}

/// The live turn's stop flag — the app side flips it on Ctrl-C; the
/// worker installs the current turn's flag here.
type StopSlot = Arc<Mutex<Option<Arc<AtomicBool>>>>;

/// Raw-mode + alternate-screen guard: restores the terminal even if the
/// app panics mid-draw (a broken terminal outlives any panic message).
struct TuiGuard;

impl TuiGuard {
    fn enter() -> std::io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut out = std::io::stdout();
        execute!(out, EnterAlternateScreen)?;
        Ok(TuiGuard)
    }
}

impl Drop for TuiGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    }
}

/// The turn worker: one Agent for the app's lifetime (kernel session +
/// continuation context chain across prompts), fed prompts over a
/// channel, emitting serialized LoopEvents back.
fn spawn_turn_worker(
    cwd: std::path::PathBuf,
    provider: Option<String>,
    model: Option<String>,
) -> (std::sync::mpsc::Sender<ToWorker>, Arc<Mutex<Scrollback>>, Arc<AtomicBool>, StopSlot) {
    let (tx, rx) = std::sync::mpsc::channel::<ToWorker>();
    let scrollback = Arc::new(Mutex::new(Scrollback::new()));
    let running = Arc::new(AtomicBool::new(false));
    let stop_slot: StopSlot = Arc::new(Mutex::new(None));
    let sc = Arc::clone(&scrollback);
    let run_flag = Arc::clone(&running);
    let stop_slot2 = Arc::clone(&stop_slot);

    std::thread::spawn(move || {
        // sampler: real provider when asked, else the offline demo planner
        let sampler: Arc<dyn okra_providers::Sampler> = match &provider {
            Some(p) if p == "openai" => {
                let model = model.clone().unwrap_or_else(|| "gpt-4o-mini".into());
                match okra_providers::OpenAiProvider::from_env(model) {
                    Some(prov) => Arc::new(prov),
                    None => {
                        if let Ok(mut sc) = sc.lock() {
                            sc.feed(
                                "error",
                                &serde_json::json!({ "message":
                                    "OKRA_API_KEY not set; falling back to the offline demo planner" }),
                            );
                        }
                        Arc::new(crate::demo_sampler::DemoPlanner::new(cwd.clone()))
                    }
                }
            }
            _ => Arc::new(crate::demo_sampler::DemoPlanner::new(cwd.clone())),
        };

        let sessions_dir = cwd.join(".okra-sessions");
        let session_id = format!("tui-{}", crate::serve::uuid_v4());
        let header = okra_kernel::SessionHeader {
            version: okra_kernel::SESSION_FORMAT_VERSION,
            id: format!("session-{session_id}"),
            created_at: okra_kernel::wall_clock(),
            cwd: cwd.to_string_lossy().into_owned(),
            parent_session: None,
            is_seeded: false,
        };
        if let Err(e) = okra_kernel::SessionHandle::create(&sessions_dir, &header) {
            if let Ok(mut sc) = sc.lock() {
                sc.feed("error", &serde_json::json!({ "message": format!("session: {e}") }));
            }
            return;
        }

        let (max_steps, clamped) =
            okra_host::managed_policy::runtime_pin().clamp_max_turns(32);
        if clamped
            && let Ok(mut sc) = sc.lock()
        {
            sc.feed("compaction_notice", &serde_json::json!({
                "note": format!("pin clamped max-turns to {max_steps}") }));
        }
        let config = okra_agent_core::loop_::AgentConfig {
            max_steps: max_steps as usize,
            unattended: true,
            ..Default::default()
        };

        let mut turn_counter = 0u64;
        let mut continuation = okra_compaction::SessionContext::default();
        let home = okra_host::fsutil::home_dir().unwrap_or_else(|| cwd.clone());
        let memory = okra_memory::TieredReader::new(home, cwd.clone());
        let skills = okra_memory::SkillCatalog::load_dir(&cwd.join(".okra").join("skills"));

        while let Ok(msg) = rx.recv() {
            match msg {
                ToWorker::Stop => break,
                ToWorker::Send(prompt) => {
                    run_flag.store(true, Ordering::Relaxed);
                    if let Ok(mut sc) = sc.lock() {
                        sc.feed("user_message", &serde_json::json!({ "text": prompt }));
                    }
                    // fresh executor + kernel handle per turn (the same
                    // open-or-create pattern as the daemon): the four-tool
                    // plane, unattended ceiling — the TUI has no approval
                    // surface yet (the composer is the only interaction)
                    let registry = crate::serve::build_registry(&cwd);
                    let approvals =
                        okra_policy::approval::ApprovalService::new(okra_policy::approval::ApprovalPolicy::Never);
                    let mut executor =
                        okra_agent_core::loop_::PolicyToolExecutor::new(registry, approvals);
                    executor.ceiling = okra_policy::ToolApprovalCeiling::UnattendedAllowed;
                    let kernel_id = format!("session-{session_id}");
                    let kernel_session =
                        match okra_kernel::SessionHandle::open(&sessions_dir, &kernel_id, okra_kernel::SessionAccess::Write)
                            .or_else(|_| okra_kernel::SessionHandle::create(&sessions_dir, &header)) {
                            Ok(h) => h,
                            Err(e) => {
                                if let Ok(mut sc) = sc.lock() {
                                    sc.feed("error", &serde_json::json!({ "message": format!("session: {e}") }));
                                }
                                run_flag.store(false, Ordering::Relaxed);
                                continue;
                            }
                        };
                    let stop = Arc::new(AtomicBool::new(false));
                    if let Ok(mut slot) = stop_slot2.lock() {
                        *slot = Some(Arc::clone(&stop));
                    }
                    let mut agent = okra_agent_core::loop_::Agent::new(
                        config.clone(),
                        Arc::clone(&sampler),
                        Box::new(executor),
                        kernel_session,
                    );
                    agent.set_turn_counter(turn_counter);
                    agent.set_stop_flag(Arc::clone(&stop));
                    let sc2 = Arc::clone(&sc);
                    let outcome = agent.run_turn_continuation(
                        &mut continuation,
                        &okra_compaction::ScriptedCompactor,
                        Some(&memory),
                        Some(&skills),
                        &prompt,
                        &mut |ev: okra_agent_core::loop_::LoopEvent| {
                            if let Ok(v) = serde_json::to_value(&ev)
                                && let Ok(mut sc) = sc2.lock()
                            {
                                let name = v["event"].as_str().unwrap_or_default().to_string();
                                sc.feed(&name, &v);
                            }
                        },
                    );
                    turn_counter += 1;
                    let _ = outcome;
                    if let Ok(mut slot) = stop_slot2.lock() {
                        *slot = None;
                    }
                    run_flag.store(false, Ordering::Relaxed);
                }
            }
        }
    });
    (tx, scrollback, running, stop_slot)
}

pub fn run_tui(
    cwd: std::path::PathBuf,
    provider: Option<String>,
    model: Option<String>,
) -> Result<(), String> {
    let _guard = TuiGuard::enter().map_err(|e| format!("terminal setup: {e}"))?;
    let (worker_tx, scrollback, running, stop_slot) =
        spawn_turn_worker(cwd, provider, model);

    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = ratatui::Terminal::new(backend).map_err(|e| format!("terminal: {e}"))?;
    let mut input = String::new();
    let mut quit = false;

    while !quit {
        let height = {
            let chunks_height = terminal.size().map(|s| s.height).unwrap_or(24);
            chunks_height.saturating_sub(2) as usize // composer + status
        };
        terminal.draw(|frame| draw(frame, &scrollback, &input, &running, height))
            .map_err(|e| format!("draw: {e}"))?;

        if crossterm::event::poll(Duration::from_millis(50)).unwrap_or(false) {
            let Ok(Event::Key(key)) = crossterm::event::read() else { continue };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match (key.code, key.modifiers) {
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                    if running.load(Ordering::Relaxed) {
                        // stop the LIVE turn (the worker stays for the next
                        // prompt); Ctrl-C while idle quits
                        if let Ok(slot) = stop_slot.lock()
                            && let Some(flag) = slot.as_ref()
                        {
                            flag.store(true, Ordering::Relaxed);
                        }
                    } else {
                        quit = true;
                    }
                }
                (KeyCode::Char('d'), KeyModifiers::CONTROL) => quit = true,
                (KeyCode::Enter, _) => {
                    let prompt = input.trim().to_string();
                    if prompt.is_empty() {
                        continue;
                    }
                    input.clear();
                    let _ = worker_tx.send(ToWorker::Send(prompt));
                }
                (KeyCode::Backspace, _) => {
                    input.pop();
                }
                (KeyCode::Char(ch), m) if m.is_empty() || m == KeyModifiers::SHIFT => {
                    input.push(ch);
                }
                (KeyCode::PageUp, _) => {
                    if let Ok(mut sc) = scrollback.lock() {
                        sc.scroll_up(height / 2, height);
                    }
                }
                (KeyCode::PageDown, _) => {
                    if let Ok(mut sc) = scrollback.lock() {
                        sc.scroll_down(height / 2, height);
                    }
                }
                (KeyCode::Char('k'), m) if m.contains(KeyModifiers::CONTROL) => {
                    if let Ok(mut sc) = scrollback.lock() {
                        sc.scroll_up(1, height);
                    }
                }
                (KeyCode::Char('G'), _) => {
                    if let Ok(mut sc) = scrollback.lock() {
                        sc.jump_to_bottom();
                    }
                }
                (KeyCode::Home, _) => {
                    if let Ok(mut sc) = scrollback.lock() {
                        sc.scroll_up(usize::MAX, height);
                    }
                }
                (KeyCode::End, _) => {
                    if let Ok(mut sc) = scrollback.lock() {
                        sc.jump_to_bottom();
                    }
                }
                _ => {}
            }
        }
    }
    let _ = worker_tx.send(ToWorker::Stop);
    let mut out = std::io::stdout();
    let _ = queue!(out, crossterm::cursor::Show);
    let _ = out.flush();
    Ok(())
}

fn draw(
    frame: &mut Frame,
    scrollback: &Arc<Mutex<Scrollback>>,
    input: &str,
    running: &Arc<AtomicBool>,
    height: usize,
) {
    let area = frame.area();
    let sc = scrollback.lock();
    let sc = match sc {
        Ok(sc) => sc,
        Err(_) => return,
    };
    let lines: Vec<UiLine> = sc
        .viewport(height)
        .iter()
        .map(|l| match l.kind {
            LineKind::Divider => UiLine::styled(
                l.text.clone(),
                Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
            ),
            LineKind::User => UiLine::styled(
                format!("❯ {}", l.text),
                Style::new().fg(Color::Cyan),
            ),
            LineKind::Assistant => UiLine::raw(l.text.clone()),
            LineKind::Tool => UiLine::styled(
                l.text.clone(),
                Style::new().fg(if l.text.starts_with('✗') { Color::Red } else { Color::Green }),
            ),
            LineKind::Note => UiLine::styled(
                l.text.clone(),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::DIM),
            ),
        })
        .collect();

    let chunks = ratatui::layout::Layout::vertical([
        ratatui::layout::Constraint::Min(1),
        ratatui::layout::Constraint::Length(1),
        ratatui::layout::Constraint::Length(1),
    ])
    .split(area);
    let mut chunks = chunks.iter();

    let Some(scroll_area) = chunks.next() else { return };
    let Some(status_area) = chunks.next() else { return };
    let Some(composer_area) = chunks.next() else { return };

    frame.render_widget(Paragraph::new(lines), *scroll_area);

    let status = if running.load(Ordering::Relaxed) {
        "running · Ctrl-C stops the turn"
    } else if sc.pinned() {
        "idle · Enter sends · PgUp scrolls · Ctrl-C quits"
    } else {
        "scrolled · G jumps to the bottom"
    };
    frame.render_widget(
        Paragraph::new(UiLine::styled(
            format!(" {status}"),
            Style::new().fg(Color::DarkGray),
        )),
        *status_area,
    );
    frame.render_widget(Paragraph::new(format!(" ❯ {input}")), *composer_area);
}

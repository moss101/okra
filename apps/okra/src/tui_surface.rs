//! The TUI as a daemon SURFACE (M4/G4: one live session visible and
//! steerable from browser + TUI simultaneously). This client speaks the
//! daemon's NDJSON v4 protocol over TCP — hello, `v4/conversation/
//! subscribe`, `v4/command` (sendText steers a running turn, stop,
//! resolveApproval) — and renders `v4/projection` rows into the pager's
//! scrollback. It is deliberately tty-free: the ratatui loop drives it,
//! and `--smoke` drives it headless for CI.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use okra_tui::pager::{LineKind, Scrollback};

pub struct SurfaceClient {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    next_id: u64,
    pub session_id: String,
    pub scrollback: Scrollback,
    /// (approvalId, tool) of the pending ask, if any.
    pub awaiting_approval: Option<(String, String)>,
    /// last seen control phase (idle/running/completed*/…)
    pub phase: String,
    /// row ids already rendered as complete lines
    rendered: std::collections::HashSet<u64>,
    /// the streaming assistant row currently owning the live line
    live_assistant_row: Option<u64>,
}

impl SurfaceClient {
    /// Attach and (optionally) create the session. Returns the client
    /// once the subscription is live.
    pub fn attach(addr: &str, session: Option<&str>) -> Result<SurfaceClient, String> {
        let stream = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .map_err(|e| format!("socket options: {e}"))?;
        let writer = stream.try_clone().map_err(|e| e.to_string())?;
        let mut client = SurfaceClient {
            reader: BufReader::new(stream),
            writer,
            next_id: 1,
            session_id: String::new(),
            scrollback: Scrollback::new(),
            awaiting_approval: None,
            phase: "idle".into(),
            rendered: std::collections::HashSet::new(),
            live_assistant_row: None,
        };

        let hello = client.call("hello", serde_json::json!({}))?;
        if hello["result"]["daemon"].as_str() != Some("okra") {
            return Err(format!("unexpected daemon: {hello}"));
        }

        let session_id = match session {
            Some(s) => s.to_string(),
            None => {
                let cmd_id = format!("tui-create-{}", client.next_id);
                let created = client.call(
                    "v4/command",
                    serde_json::json!({
                        "envelope": {
                            "commandId": cmd_id,
                            "type": "createSession",
                            "payload": {}
                        }
                    }),
                )?;
                created["result"]["result"]["sessionId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            }
        };
        if session_id.is_empty() {
            return Err("no session id (createSession failed?)".into());
        }
        client
            .call(
                "v4/conversation/subscribe",
                serde_json::json!({ "sessionId": session_id }),
            )
            .map_err(|e| format!("subscribe: {e}"))?;
        client.session_id = session_id;
        Ok(client)
    }

    fn send_raw(&mut self, msg: &serde_json::Value) -> Result<(), String> {
        let line = serde_json::to_string(msg).unwrap_or_default();
        self.writer
            .write_all(format!("{line}\n").as_bytes())
            .map_err(|e| format!("send: {e}"))?;
        self.writer.flush().ok();
        Ok(())
    }

    /// RPC: send, wait for the reply with our id, absorbing any
    /// notifications that arrive first (projections keep rendering).
    fn call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = serde_json::json!({ "id": id, "method": method, "params": params });
        self.send_raw(&msg)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if Instant::now() > deadline {
                return Err(format!("timeout waiting for reply {id}"));
            }
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => return Err("daemon closed the connection".into()),
                Ok(_) => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(e) => return Err(format!("read: {e}")),
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                if v["id"].as_u64() == Some(id) {
                    return Ok(v);
                }
                self.absorb(v);
            }
        }
    }

    /// Drain whatever frames are available (bounded) into the
    /// scrollback; returns how many frames were consumed.
    pub fn poll(&mut self) -> usize {
        let mut consumed = 0;
        let deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < deadline {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(_) => break,
            }
            if let Ok(msg) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                consumed += 1;
                self.absorb(msg);
            }
        }
        consumed
    }

    fn absorb(&mut self, msg: serde_json::Value) {
        let method = msg["method"].as_str().unwrap_or_default();
        if method != "v4/projection" {
            return;
        }
        let params = &msg["params"];
        if let Some(phase) = params["control"]["phase"].as_str() {
            self.phase = phase.to_string();
        }
        if let Some(asks) = params["control"]["awaitingApproval"].as_array()
            && let Some(ask) = asks.first()
        {
            let id = ask["approvalId"].as_str().unwrap_or_default().to_string();
            let tool = ask["tool"].as_str().unwrap_or("tool").to_string();
            if self
                .awaiting_approval
                .as_ref()
                .is_none_or(|(i, _)| i != &id)
            {
                self.scrollback
                    .push_line(LineKind::Note, &format!("! approval asked: {tool} [y/n]"));
            }
            self.awaiting_approval = Some((id, tool));
        } else if self.awaiting_approval.is_some() {
            self.awaiting_approval = None;
        }
        let Some(rows) = params["rows"].as_array() else { return };
        for row in rows {
            let Some(row_id) = row["rowId"].as_u64() else { continue };
            let kind = row["kind"].as_str().unwrap_or_default();
            match kind {
                // streaming assistant rows own the live line: each
                // revision carries the full text, so REPLACE the line
                "assistantText" => {
                    if let Some(rendered) = okra_tui::projection_row_line(row) {
                        self.live_assistant_row = Some(row_id);
                        self.scrollback.set_last_line(LineKind::Assistant, &rendered);
                    }
                    if row["state"] != "streaming" {
                        self.rendered.insert(row_id);
                        self.live_assistant_row = None;
                    }
                }
                _ => {
                    if self.rendered.contains(&row_id) {
                        continue;
                    }
                    if let Some(line) = okra_tui::projection_row_line(row) {
                        let line_kind = match kind {
                            "userInput" => LineKind::User,
                            "turnHeader" => LineKind::Divider,
                            "toolCall" => LineKind::Tool,
                            _ => LineKind::Note,
                        };
                        self.scrollback.push_line(line_kind, &line);
                        self.rendered.insert(row_id);
                    }
                }
            }
        }
    }

    fn command(&mut self, cmd_type: &str, payload: serde_json::Value) -> Result<(), String> {
        let id = self.next_id;
        self.next_id += 1;
        // the NDJSON v4/command wraps the envelope one level down
        self.send_raw(&serde_json::json!({
            "id": id,
            "method": "v4/command",
            "params": {
                "envelope": {
                    "commandId": format!("tui-{id}"),
                    "type": cmd_type,
                    "sessionId": self.session_id,
                    "payload": payload,
                }
            }
        }))
    }

    /// Send text: starts a turn when idle, STEERS the live one when
    /// running (the daemon's running-turn gate routes it).
    pub fn send_text(&mut self, text: &str) -> Result<(), String> {
        self.command("sendText", serde_json::json!({ "text": text }))
    }

    pub fn stop(&mut self) -> Result<(), String> {
        self.command("stop", serde_json::json!({}))
    }

    pub fn resolve_approval(&mut self, approval_id: &str, allow: bool) -> Result<(), String> {
        self.command(
            "resolveApproval",
            serde_json::json!({
                "approvalId": approval_id,
                "decision": if allow { "allow" } else { "deny" }
            }),
        )
    }

    /// Headless drive for CI (`--smoke`): subscribe, send, poll until
    /// the turn reaches a terminal phase (resolving approvals with
    /// `allow` unless `deny_approvals`), then return so the caller
    /// prints the scrollback.
    pub fn smoke(&mut self, prompt: &str, deny_approvals: bool, timeout: Duration) -> Result<(), String> {
        self.send_text(prompt)?;
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() > deadline {
                return Err(format!("turn never reached a terminal phase (last: {})", self.phase));
            }
            self.poll();
            if let Some((id, _tool)) = self.awaiting_approval.clone() {
                let _ = self.resolve_approval(&id, !deny_approvals);
            }
            if self.phase.starts_with("completed") || self.phase == "failed" {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

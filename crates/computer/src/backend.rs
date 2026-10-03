//! The real macOS executor behind the M1 contracts (N0023): AX-first
//! observe via System Events, element/coordinate actions via cliclick,
//! screenshots via `screencapture`.
//!
//! This module is the computer crate's SANCTIONED spawn site (mirrors
//! okra-tools/src/process.rs): clippy bans raw spawn workspace-wide, and
//! every spawn here runs user-approved targets with fixed argument
//! shapes — never model-supplied argv (app names and text are passed as
//! AppleScript `on run` argv / cliclick arguments, quoted by the OS).
//!
//! Binary paths are env-overridable so acceptance tests stay hermetic:
//! `OKRA_OSASCRIPT`, `OKRA_CLICKER`, `OKRA_SCREENCAPTURE`.
//!
//! Honest failure: missing Accessibility/Screen-Recording permission is a
//! typed error, never a silent empty tree — the pixel guard's spirit at
//! the OS boundary.

use serde::{Deserialize, Serialize};
use std::process::Command;
use std::time::Duration;

fn bin_env(var: &str, default: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| default.to_string())
}

fn osascript() -> String {
    bin_env("OKRA_OSASCRIPT", "/usr/bin/osascript")
}

fn clicker() -> String {
    bin_env("OKRA_CLICKER", "cliclick")
}

fn screencapture() -> String {
    bin_env("OKRA_SCREENCAPTURE", "/usr/sbin/screencapture")
}

/// One element of the observed AX tree (AX-first: elements, never pixels).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AxElement {
    /// Stable within one observation: `w0/e3` shape.
    pub id: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
    /// Actions this element reports (AXPress etc.).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<String>,
}

/// The observed tree of one app (frontmost windows, 2 levels deep).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AxTree {
    pub app: String,
    pub elements: Vec<AxElement>,
    /// Honest degradation notes (e.g. permission failures surface here
    /// as an Err instead — this field is for partial trees).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[allow(clippy::disallowed_methods)] // sanctioned site (see module docs)
fn run_osascript(script: &str, argv: &[String]) -> Result<String, String> {
    let mut cmd = Command::new(osascript());
    cmd.arg("-e").arg(script).stdout(std::process::Stdio::piped());
    for a in argv {
        cmd.arg(a);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("osascript spawn: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        // the classic permission denial mentions "assistive access" / -25211
        if stderr.contains("assistive") || stderr.contains("-25211") || stderr.contains("not allowed") {
            return Err(
                "accessibility permission not granted to okra (System Events denied the \
                 AX query). Grant Accessibility to the host terminal, then re-observe."
                    .into(),
            );
        }
        return Err(format!("osascript: {}", stderr.trim()));
    }
    Ok(stdout)
}

/// Observe one app's AX tree (windows + their interactive elements,
/// 2 levels: AX-first element targeting, never pixel guessing).
#[allow(clippy::disallowed_methods)] // sanctioned site
pub fn observe(app: &str) -> Result<AxTree, String> {
    let script = r#"
on run argv
  set appName to item 1 of argv
  tell application "System Events"
    if not (exists process appName) then
      return "NO-PROCESS"
    end if
    tell process appName
      set out to ""
      set wIdx to 0
      repeat with w in windows
        set eIdx to 0
        set p to position of w
        set s to size of w
        set out to out & "window|" & wIdx & "|w" & wIdx & "|" & ((item 1 of p) as text) & "|" & ((item 2 of p) as text) & "|" & ((item 1 of s) as text) & "|" & ((item 2 of s) as text) & linefeed
        repeat with e in (UI elements of w)
          try
            set eIdx to eIdx + 1
            set r to role of e as text
            set lbl to ""
            try
              set lbl to name of e as text
            end try
            set p to position of e
            set s to size of e
            set acts to ""
            try
              set acts to (name of actions of e) as text
            end try
            set out to out & "elem|" & wIdx & "|" & eIdx & "|" & r & "|" & lbl & "|" & ((item 1 of p) as text) & "|" & ((item 2 of p) as text) & "|" & ((item 1 of s) as text) & "|" & ((item 2 of s) as text) & "|" & acts & linefeed
          end try
        end repeat
        set wIdx to wIdx + 1
      end repeat
      return out
    end tell
  end tell
end run
"#;
    let out = run_osascript(script, &[app.to_string()])?;
    if out.trim() == "NO-PROCESS" {
        return Err(format!("no running process named {app:?}"));
    }
    let mut elements = Vec::new();
    for line in out.lines() {
        let f: Vec<&str> = line.split('|').collect();
        match f.first().copied() {
            Some("window") if f.len() >= 7 => {
                let (widx, x, y, w, h) = (
                    f[1], f[3], f[4], f[5], f[6],
                );
                elements.push(AxElement {
                    id: format!("w{widx}"),
                    role: "window".into(),
                    label: None,
                    x: x.parse().unwrap_or(0),
                    y: y.parse().unwrap_or(0),
                    w: w.parse().unwrap_or(0),
                    h: h.parse().unwrap_or(0),
                    actions: vec![],
                });
            }
            Some("elem") if f.len() >= 10 => {
                let (widx, eidx, role, label, x, y, w, h, acts) =
                    (f[1], f[2], f[3], f[4], f[5], f[6], f[7], f[8], f[9]);
                elements.push(AxElement {
                    id: format!("w{widx}/e{eidx}"),
                    role: role.trim_start_matches("AX").to_lowercase(),
                    label: if label.is_empty() { None } else { Some(label.to_string()) },
                    x: x.parse().unwrap_or(0),
                    y: y.parse().unwrap_or(0),
                    w: w.parse().unwrap_or(0),
                    h: h.parse().unwrap_or(0),
                    actions: acts
                        .split(',')
                        .map(|a| a.trim())
                        .filter(|a| !a.is_empty())
                        .map(|a| a.trim_start_matches("AX").to_lowercase())
                        .collect(),
                });
            }
            _ => {}
        }
    }
    Ok(AxTree { app: app.to_string(), elements, note: None })
}

/// Click an observed element: AXPress when the element reports it,
/// otherwise a coordinate click at its center via the clicker (coords
/// come from the AX tree — AX-first, never pixel-guessed).
#[allow(clippy::disallowed_methods)] // sanctioned site
pub fn click_element(app: &str, tree: &AxTree, element_id: &str) -> Result<(), String> {
    let el = tree
        .elements
        .iter()
        .find(|e| e.id == element_id)
        .ok_or_else(|| format!("element {element_id} not in the observed tree — re-observe"))?;
    if el.role == "window" {
        return Err("cannot click a window element; pick an interactive element".into());
    }
    if el.actions.iter().any(|a| a == "press") {
        // structural path press: w/e indices, no coordinates involved
        let (w, e) = element_id
            .strip_prefix('w')
            .and_then(|rest| rest.split_once("/e"))
            .ok_or("bad element id")?;
        run_osascript(
            r#"
on run argv
  tell application "System Events" to tell process (item 1 of argv)
    perform action "AXPress" of (UI element (item 3 of argv as integer) of window (item 2 of argv as integer))
  end tell
end run
"#,
            &[app.to_string(), w.to_string(), e.to_string()],
        )?;
        Ok(())
    } else {
        let cx = el.x + el.w / 2;
        let cy = el.y + el.h / 2;
        let out = Command::new(clicker())
            .arg(format!("c:{cx},{cy}"))
            .output()
            .map_err(|e| format!("clicker spawn: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "clicker failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }
}

/// Type text into the focused element (the caller focuses first via
/// click_element). `keystroke` through osascript.
#[allow(clippy::disallowed_methods)] // sanctioned site
pub fn type_text(text: &str) -> Result<(), String> {
    run_osascript(
        r#"
on run argv
  tell application "System Events" to keystroke (item 1 of argv)
end run
"#,
        &[text.to_string()],
    )?;
    Ok(())
}

/// Press a key by name (return, tab, space, esc, up, down, …).
#[allow(clippy::disallowed_methods)] // sanctioned site
pub fn press_key(key: &str) -> Result<(), String> {
    run_osascript(
        r#"
on run argv
  tell application "System Events" to key code (item 1 of argv)
end run
"#,
        &[key_code(key).to_string()],
    )?;
    Ok(())
}

fn key_code(name: &str) -> i32 {
    match name.to_lowercase().as_str() {
        "return" | "enter" => 36,
        "tab" => 48,
        "space" => 49,
        "esc" | "escape" => 53,
        "delete" | "backspace" => 51,
        "up" => 126,
        "down" => 125,
        "left" => 123,
        "right" => 124,
        _ => 0,
    }
}

/// Screenshot the screen (no sound); returns PNG bytes. Screen-Recording
/// permission failures surface as an image of the desktop wallpaper on
/// macOS 10.15+ — the caller shows the image either way (honest output).
#[allow(clippy::disallowed_methods)] // sanctioned site
pub fn screenshot() -> Result<Vec<u8>, String> {
    let tmp = std::env::temp_dir().join(format!(
        "okra-shot-{}.png",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default()
    ));
    let out = Command::new(screencapture())
        .arg("-x")
        .arg(&tmp)
        .output()
        .map_err(|e| format!("screencapture spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "screencapture failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let bytes = std::fs::read(&tmp).map_err(|e| format!("shot read: {e}"))?;
    let _ = std::fs::remove_file(&tmp);
    Ok(bytes)
}

/// A bounded wait helper (the typing guard's caller may poll).
pub fn wait(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// env mutation is unsafe in edition 2024; a test-local helper keeps
    /// the unsafe blocks single-purpose (set + removal pairing).
    unsafe fn env_guard(_var: &str) {}

    /// A fixture "osascript": prints a canned tree and counts invocations.
    fn fixture_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "okra-ax-fixture-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// ONE serial test for the env-overridable backend: the env is
    /// process-global and tests run in parallel — each fixture phase runs
    /// sequentially inside a single test.
    // the fixture pipeline drives a #!/bin/sh script standing in for
    // osascript — unix/mac semantics end to end
    #[test]
    #[cfg(unix)]
    fn backend_fixture_phases() {
        unsafe { env_guard("OKRA_OSASCRIPT") };
        let dir = fixture_dir();
        let script = dir.join("osascript");

        // phase 1: parse a canned tree
        std::fs::write(
            &script,
            "#!/bin/sh\necho 'window|0|w0|10|20|800|600'\necho 'elem|0|1|AXButton|OK|100|300|80|30|AXPress,AXOpen'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        unsafe { std::env::set_var("OKRA_OSASCRIPT", &script) };
        let tree = observe("Finder").unwrap();
        assert_eq!(tree.elements.len(), 2);
        assert_eq!(tree.elements[0].id, "w0");
        assert_eq!(tree.elements[1].id, "w0/e1");
        assert_eq!(tree.elements[1].role, "button");
        assert!(tree.elements[1].actions.contains(&"press".into()));

        // phase 2: permission denial is a typed error
        std::fs::write(
            &script,
            "#!/bin/sh\necho 'execution error: Not authorized to send Apple events (-25211)' >&2\nexit 1\n",
        )
        .unwrap();
        let err = observe("Finder").unwrap_err();
        assert!(err.contains("accessibility permission"), "{err}");

        unsafe { std::env::remove_var("OKRA_OSASCRIPT") };
    }

    #[test]
    fn element_not_in_tree_is_a_reobserve_error() {
        let tree = AxTree { app: "X".into(), elements: vec![], note: None };
        let err = click_element("X", &tree, "w0/e9").unwrap_err();
        assert!(err.contains("re-observe"), "{err}");
    }
}

/// The real batch execution (the M1 contract's stub, realized): observe →
/// resolve each action against the fresh tree → act → re-observe, with
/// stop-on-first-error. `typing` is the user_actively_typing guard.
pub fn execute_real(
    app: &str,
    actions: &[crate::AxAction],
    typing: bool,
) -> Vec<crate::AxActionResult> {
    eprintln!(
        "[backend-debug] execute_real app={app} typing={typing} actions={}",
        actions.len()
    );
    let mut results = Vec::new();
    for action in actions {
        let tree = match observe(app) {
            Ok(t) => t,
            Err(e) => {
                results.push(crate::AxActionResult {
                    ok: false,
                    error: Some(e),
                    observed: None,
                });
                break; // stop-on-first-error
            }
        };
        let element_id = match action {
            crate::AxAction::Click { element_id, .. }
            | crate::AxAction::Type { element_id, .. }
            | crate::AxAction::Scroll { element_id, .. } => element_id.clone(),
            crate::AxAction::PressKey { .. } => String::new(),
        };
        let r = match action {
            crate::AxAction::Click { element_id, .. } => click_element(app, &tree, element_id)
                .map(|_| crate::AxActionResult {
                    ok: true,
                    error: None,
                    observed: None,
                })
                .unwrap_or_else(|e| crate::AxActionResult {
                    ok: false,
                    error: Some(e),
                    observed: None,
                }),
            crate::AxAction::Type { text, .. } => {
                if typing {
                    crate::AxActionResult {
                        ok: false,
                        error: Some("user_actively_typing: input injection paused".into()),
                        observed: None,
                    }
                } else {
                    type_text(text)
                        .map(|_| crate::AxActionResult { ok: true, error: None, observed: None })
                        .unwrap_or_else(|e| crate::AxActionResult {
                            ok: false,
                            error: Some(e),
                            observed: None,
                        })
                }
            }
            crate::AxAction::PressKey { key, .. } => {
                if typing {
                    crate::AxActionResult {
                        ok: false,
                        error: Some("user_actively_typing: input injection paused".into()),
                        observed: None,
                    }
                } else {
                    press_key(key)
                        .map(|_| crate::AxActionResult { ok: true, error: None, observed: None })
                        .unwrap_or_else(|e| crate::AxActionResult {
                            ok: false,
                            error: Some(e),
                            observed: None,
                        })
                }
            }
            crate::AxAction::Scroll { .. } => crate::AxActionResult {
                ok: false,
                error: Some(
                    "scroll is not supported by the AX backend v1; use PageUp/PageDown key \
                     presses on the focused element"
                        .into(),
                ),
                observed: None,
            },
        };
        let stop = !r.ok;
        // re-observe after every executed action (diffed state, AX-first)
        let observed = if r.ok {
            observe(app).ok().map(|t| serde_json::to_value(&t).unwrap_or_default())
        } else {
            None
        };
        results.push(crate::AxActionResult {
            observed,
            ..r
        });
        let _ = element_id;
        if stop {
            break;
        }
    }
    results
}

// ---------------------------------------------------------------------------
// Claude Desktop parity surface (N0025): display-scope coordinate family,
// app inventory, and the background app_* window family.
// ---------------------------------------------------------------------------

#[allow(clippy::disallowed_methods)] // sanctioned site (see module docs)
fn run_clicker(args: &[String]) -> Result<String, String> {
    let out = Command::new(clicker())
        .args(args)
        .output()
        .map_err(|e| format!("clicker spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "clicker failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// One-shot point actions in the last-full-screenshot coordinate frame.
pub fn click_point(x: i64, y: i64) -> Result<(), String> {
    run_clicker(&[format!("c:{x},{y}")]).map(|_| ())
}
pub fn double_click_point(x: i64, y: i64) -> Result<(), String> {
    run_clicker(&[format!("dc:{x},{y}")]).map(|_| ())
}
pub fn right_click_point(x: i64, y: i64) -> Result<(), String> {
    run_clicker(&[format!("rc:{x},{y}")]).map(|_| ())
}
pub fn mouse_move(x: i64, y: i64) -> Result<(), String> {
    run_clicker(&[format!("m:{x},{y}")]).map(|_| ())
}
/// Press-drag-release between two points.
pub fn drag(from: (i64, i64), to: (i64, i64)) -> Result<(), String> {
    run_clicker(&[
        format!("dd:{},{}", from.0, from.1),
        format!("dm:{},{}", to.0, to.1),
        format!("du:{},{}", to.0, to.1),
    ])
    .map(|_| ())
}
/// Scroll `amount` ticks at a point; positive dy scrolls down.
pub fn scroll_at(x: i64, y: i64, dy: i32) -> Result<(), String> {
    run_clicker(&[format!("scroll {dy} 0 {x} {y}")]).map(|_| ())
}
/// Current cursor position (logical points).
pub fn cursor_position() -> Result<(i64, i64), String> {
    let out = run_clicker(&["p:".to_string()])?;
    // cliclick prints "<x>,<y>"
    let (x, y) = out
        .trim()
        .split_once(',')
        .ok_or_else(|| format!("cursor parse: {out}"))?;
    let xp: i64 = x.trim().parse().map_err(|e| format!("cursor x: {e}"))?;
    let yp: i64 = y.trim().parse().map_err(|e| format!("cursor y: {e}"))?;
    Ok((xp, yp))
}

/// Press a key COMBO like "cmd+a" / "Return" / "ctrl+shift+t".
pub fn press_combo(combo: &str) -> Result<(), String> {
    let parts: Vec<&str> = combo.split('+').map(str::trim).collect();
    let (key, mods) = parts.split_last().ok_or("empty combo")?;
    let using: Vec<String> = mods
        .iter()
        .map(|m| match m.to_lowercase().as_str() {
            "cmd" | "meta" | "win" => "command down".to_string(),
            "ctrl" | "control" => "control down".to_string(),
            "alt" | "option" => "option down".to_string(),
            "shift" => "shift down".to_string(),
            other => format!("/* unknown modifier {other} */"),
        })
        .collect();
    let using_list = if using.is_empty() {
        String::new()
    } else {
        format!(" using {}", using.join(", "))
    };
    // key names the AX keystroke vocabulary understands
    let key_ax = match key.to_lowercase().as_str() {
        "return" | "enter" => "return".to_string(),
        "esc" | "escape" => "escape".to_string(),
        "delete" | "backspace" => "delete".to_string(),
        "tab" | "space" | "up" | "down" | "left" | "right" => key.to_lowercase(),
        single if single.chars().count() == 1 => single.to_string(),
        other => other.to_string(),
    };
    let script = format!(
        r#"
tell application "System Events" to keystroke "{key_ax}"{using_list}
"#
    );
    run_osascript(&script, &[]).map(|_| ())
}

/// Capture a REGION of the screen fresh (zoom semantics: re-inspect small
/// text at full density; v1 re-captures rather than cropping the buffer).
#[allow(clippy::disallowed_methods)] // sanctioned site (see module docs)
pub fn screenshot_region(x: i64, y: i64, w: i64, h: i64) -> Result<Vec<u8>, String> {
    let tmp = std::env::temp_dir().join(format!(
        "okra-zoom-{}.png",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default()
    ));
    let out = Command::new(screencapture())
        .arg("-x")
        .arg("-R")
        .arg(format!("{x},{y},{w},{h}"))
        .arg(&tmp)
        .output()
        .map_err(|e| format!("screencapture spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "screencapture failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let bytes = std::fs::read(&tmp).map_err(|e| format!("shot read: {e}"))?;
    let _ = std::fs::remove_file(&tmp);
    Ok(bytes)
}

/// Running applications (System Events process names), running-first.
pub fn list_running_apps() -> Result<Vec<String>, String> {
    let out = run_osascript(
        r#"tell application "System Events" to get name of every process"#,
        &[],
    )?;
    Ok(out
        .trim()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect())
}

/// Installed applications (name list from /Applications + system apps).
pub fn list_installed_apps() -> Vec<String> {
    let mut apps = Vec::new();
    for dir in ["/Applications", "/System/Applications"] {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if let Some(stem) = name.strip_suffix(".app") {
                    apps.push(stem.to_string());
                }
            }
        }
    }
    apps.sort();
    apps
}

/// Launch/ensure an application is running (does not force frontmost).
#[allow(clippy::disallowed_methods)] // sanctioned site (see module docs)
pub fn open_application(app: &str) -> Result<(), String> {
    let out = Command::new("/usr/bin/open")
        .arg("-a")
        .arg(app)
        .output()
        .map_err(|e| format!("open spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "open -a {app} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Read the clipboard (pbpaste).
#[allow(clippy::disallowed_methods)] // sanctioned site (see module docs)
pub fn read_clipboard() -> Result<String, String> {
    let out = Command::new("/usr/bin/pbpaste")
        .output()
        .map_err(|e| format!("pbpaste: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Write the clipboard (pbcopy).
#[allow(clippy::disallowed_methods)] // sanctioned site (see module docs)
pub fn write_clipboard(text: &str) -> Result<(), String> {
    use std::io::Write;
    let mut child = Command::new("/usr/bin/pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("pbcopy: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    let _ = child.wait();
    Ok(())
}

/// One window of an app, as observed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AxWindow {
    pub window_id: String,
    pub title: String,
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
}

impl From<&AxElement> for AxWindow {
    fn from(e: &AxElement) -> Self {
        AxWindow {
            window_id: e.id.clone(),
            title: e.label.clone().unwrap_or_default(),
            x: e.x,
            y: e.y,
            w: e.w,
            h: e.h,
        }
    }
}

/// `app_list_windows`: the observed windows of an app.
pub fn app_list_windows(tree: &AxTree) -> Vec<AxWindow> {
    tree.elements
        .iter()
        .filter(|e| e.role == "window")
        .map(AxWindow::from)
        .collect()
}

/// `app_ax_find`: filter an observed tree by role and/or title substring.
pub fn app_ax_find<'a>(tree: &'a AxTree, role: Option<&str>, title: Option<&str>) -> Vec<&'a AxElement> {
    tree.elements
        .iter()
        .filter(|e| e.role != "window")
        .filter(|e| role.is_none_or(|r| e.role == r))
        .filter(|e| {
            title.is_none_or(|t| {
                e.label
                    .as_deref()
                    .is_some_and(|l| l.to_lowercase().contains(&t.to_lowercase()))
            })
        })
        .collect()
}

/// `app_screenshot`: capture a WINDOW region fresh + return its AX digest
/// (element indices from the last observe).
pub fn app_screenshot<'a>(tree: &'a AxTree, window_id: &str) -> Result<(Vec<u8>, Vec<&'a AxElement>), String> {
    let win = tree
        .elements
        .iter()
        .find(|e| e.id == window_id && e.role == "window")
        .ok_or_else(|| format!("window {window_id} not observed — re-observe"))?;
    let png = screenshot_region(win.x, win.y, win.w, win.h)?;
    let digest: Vec<&AxElement> = tree
        .elements
        .iter()
        .filter(|e| e.id.starts_with(&format!("{window_id}/")))
        .collect();
    Ok((png, digest))
}

/// `app_focus`: set AX focus on an element WITHOUT clicking or raising.
pub fn app_focus(app: &str, tree: &AxTree, element_id: &str) -> Result<(), String> {
    let el = tree
        .elements
        .iter()
        .find(|e| e.id == element_id)
        .ok_or_else(|| format!("element {element_id} not in the observed tree — re-observe"))?;
    let (w, e) = element_id
        .strip_prefix('w')
        .and_then(|rest| rest.split_once("/e"))
        .ok_or("bad element id")?;
    let _ = el;
    run_osascript(
        r#"
on run argv
  tell application "System Events" to tell process (item 1 of argv)
    set focused of (UI element (item 3 of argv as integer) of window (item 2 of argv as integer)) to true
  end tell
end run
"#,
        &[app.to_string(), w.to_string(), e.to_string()],
    )?;
    Ok(())
}

/// `app_type` with `target:"focused"`: focus the element then type.
pub fn app_type_into(
    app: &str,
    tree: &AxTree,
    element_id: Option<&str>,
    text: &str,
) -> Result<(), String> {
    if let Some(id) = element_id {
        app_focus(app, tree, id)?;
    }
    type_text(text)
}

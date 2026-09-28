//! Mobile replay export — the M6 "mobile replay client" transport-free core
//! (MASTER-PLAN §4 M6). The delivery-profile concept is already in the
//! protocol (`replayable`); this module produces the replay ARTIFACT: a
//! single self-contained HTML file rendered from a kernel session log —
//! no JavaScript, no external assets, phone-friendly viewport — that any
//! mobile browser displays offline.
//!
//! Escaping is total: session content is untrusted (it quotes workspace
//! files), so every text passes through `esc` before entering the HTML.
//! The surface-event taxonomy is the kernel's (`user/message`,
//! `assistant/message`, `tool/result` are surface; `tool/call` and other
//! log-only types are rendered as machinery rows).

use std::path::Path;

use okra_kernel as kernel;

#[derive(Debug, Clone, PartialEq)]
pub struct ReplayError(pub String);

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "replay export: {}", self.0)
    }
}

impl std::error::Error for ReplayError {}

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn text_of(data: &serde_json::Value) -> String {
    data.get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// One rendered transcript row.
pub struct ReplayRow {
    pub role: &'static str,
    pub label: String,
    pub body: String,
}

pub fn render_rows(events: &[kernel::SessionEvent]) -> Vec<ReplayRow> {
    let mut rows = Vec::new();
    for ev in events {
        match ev.event_type.as_str() {
            "user/message" => rows.push(ReplayRow {
                role: "user",
                label: "user".into(),
                body: text_of(&ev.data),
            }),
            "assistant/message" => rows.push(ReplayRow {
                role: "assistant",
                label: "assistant".into(),
                body: text_of(&ev.data),
            }),
            "tool/call" => {
                let tool = ev
                    .data
                    .get("tool")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("tool");
                rows.push(ReplayRow {
                    role: "tool",
                    label: "tool call".into(),
                    body: tool.to_string(),
                });
            }
            "tool/result" => {
                // real logs carry `output` (loop_.rs logs the model-visible
                // text); the `text` fallback covers pre-field logs/fixtures
                let body = ev
                    .data
                    .get("output")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| text_of(&ev.data));
                rows.push(ReplayRow {
                    role: "tool",
                    label: "tool result".into(),
                    body,
                });
            }
            other => rows.push(ReplayRow {
                role: "system",
                label: other.to_string(),
                body: ev.data.to_string(),
            }),
        }
    }
    rows
}

const STYLE: &str = "body{font-family:system-ui,-apple-system,sans-serif;background:#14161a;color:#d6dae0;margin:0;padding:1rem}\
main{max-width:42rem;margin-inline:auto}\
h1{font-size:1rem}\
.meta{color:#8a919c;font-size:.75rem;margin-bottom:1rem}\
.row{border-left:3px solid #2a2e35;border-radius:4px;background:#101216;padding:.4rem .6rem;margin:.45rem 0;white-space:pre-wrap;word-break:break-word;font-size:.85rem}\
.row .label{display:block;font-size:.6rem;text-transform:uppercase;letter-spacing:.05em;color:#8a919c;margin-bottom:.15rem}\
.row.user{border-left-color:#2f6feb}\
.row.assistant{border-left-color:#4fae54}\
.row.tool{border-left-color:#b58a3c}\
.empty{color:#8a919c}";

/// Render events into a standalone HTML transcript.
pub fn render_events_html(session_id: &str, events: &[kernel::SessionEvent]) -> String {
    let rows = render_rows(events);
    let body = if rows.is_empty() {
        format!("<p class=\"empty\">no events in session {}</p>", esc(session_id))
    } else {
        rows.iter()
            .map(|r| {
                format!(
                    "<div class=\"row {}\"><span class=\"label\">{}</span>{}</div>\n",
                    r.role,
                    esc(&r.label),
                    esc(&r.body)
                )
            })
            .collect()
    };
    format!(
        "<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>okra replay: {}</title>\n<style>{}</style>\n</head>\n<body>\n\
         <main>\n<h1>okra replay</h1>\n<p class=\"meta\">session {} · {} events · \
         exported by okra {}</p>\n{}</main>\n</body>\n</html>\n",
        esc(session_id),
        STYLE,
        esc(session_id),
        events.len(),
        env!("CARGO_PKG_VERSION"),
        body
    )
}

/// Export a kernel session as a standalone HTML replay.
pub fn export_session_replay(sessions_root: &Path, session_id: &str) -> Result<String, ReplayError> {
    let handle = kernel::SessionHandle::open(
        sessions_root,
        session_id,
        kernel::SessionAccess::Read,
    )
    .map_err(|e| ReplayError(format!("open session {session_id}: {e}")))?;
    let events = handle
        .read_all()
        .map_err(|e| ReplayError(format!("read session {session_id}: {e}")))?;
    Ok(render_events_html(session_id, &events))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(ty: &str, data: serde_json::Value) -> kernel::SessionEvent {
        kernel::make_event(ty, data, || 1_789_510_400_123.0)
    }

    #[test]
    fn rows_map_surface_and_machinery_events() {
        let events = vec![
            ev("user/message", serde_json::json!({"text": "hello", "origin": "user"})),
            kernel::make_log_only_event(
                "tool/call",
                serde_json::json!({"callId": "c1", "tool": "read_file"}),
                || 0.0,
            ),
            ev("tool/result", serde_json::json!({"text": "file body"})),
            ev("assistant/message", serde_json::json!({"text": "done"})),
        ];
        let rows = render_rows(&events);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].role, "user");
        assert_eq!(rows[0].body, "hello");
        assert_eq!(rows[1].label, "tool call");
        assert_eq!(rows[1].body, "read_file");
        assert_eq!(rows[2].body, "file body");
        assert_eq!(rows[3].role, "assistant");
    }

    #[test]
    fn tool_result_prefers_output_over_legacy_text() {
        let with_output = render_rows(&[ev(
            "tool/result",
            serde_json::json!({"callId": "c1", "isError": false, "output": "the REAL body"}),
        )]);
        assert_eq!(with_output[0].body, "the REAL body");
        // legacy `text` (pre-field logs/fixtures) still renders
        let legacy = render_rows(&[ev(
            "tool/result",
            serde_json::json!({"callId": "c1", "text": "legacy body"}),
        )]);
        assert_eq!(legacy[0].body, "legacy body");
        // neither → empty body, not a panic
        let bare = render_rows(&[ev(
            "tool/result",
            serde_json::json!({"callId": "c1", "isError": true}),
        )]);
        assert_eq!(bare[0].body, "");
    }

    #[test]
    fn html_is_standalone_and_escapes_content() {
        let events = vec![
            ev("user/message", serde_json::json!({"text": "<script>alert(1)</script>"})),
            ev("assistant/message", serde_json::json!({"text": "a & b \"quoted\""})),
        ];
        let html = render_events_html("sess/..\\evil", &events);
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("viewport"));
        assert!(!html.contains("<script>alert"), "content must be escaped: {html}");
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("a &amp; b &quot;quoted&quot;"));
        // session id is escaped too (it can carry path junk)
        // the id passes through escaped-for-HTML only; backslash is not an
        // HTML metacharacter so it remains literal
        assert!(html.contains("sess/..\\evil"), "{html}");
        assert!(!html.contains("<script src"), "no external assets");
    }

    #[test]
    fn empty_session_renders_placeholder() {
        let html = render_events_html("empty-session", &[]);
        assert!(html.contains("no events"));
    }
}

//! okra-tui — the `--minimal` scrollback mode (MASTER-PLAN §3 #55).
//! grok's ratatui pager lands at M4; M1 provides the plain-terminal writer
//! that renders LoopEvents into scrollback-friendly lines with O(N)
//! streaming (no block re-render).

/// Render a protocol-style event value into scrollback lines.
pub fn minimal_line(event: &str, payload: &serde_json::Value) -> Option<String> {
    match event {
        "turn_started" => Some("── turn ──".into()),
        "text_delta" => payload.get("text").and_then(|t| t.as_str()).map(str::to_string),
        "tool_call_started" => Some(format!(
            "▸ {}",
            payload.get("name").and_then(|n| n.as_str()).unwrap_or("tool")
        )),
        "tool_call_finished" => {
            let name = payload.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
            let err = payload.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
            let out = payload.get("output").and_then(|o| o.as_str()).unwrap_or("");
            let marker = if err { "✗" } else { "✓" };
            Some(format!("{marker} {name}: {}", out.lines().next().unwrap_or("")))
        }
        "turn_finished" => Some("── end ──".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_scrollback_lines() {
        assert_eq!(
            minimal_line("tool_call_started", &json!({"name": "read_file"})).unwrap(),
            "▸ read_file"
        );
        assert_eq!(
            minimal_line("tool_call_finished", &json!({"name": "read_file", "is_error": false, "output": "ok\nmore"}))
                .unwrap(),
            "✓ read_file: ok"
        );
        assert!(minimal_line("phase", &json!({"phase": "Streaming"})).is_none());
    }
}

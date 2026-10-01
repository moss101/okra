//! The pager's scrollback model (MASTER-PLAN §3 #55, grok pager analog):
//! block-structured, O(N) streaming, no terminal IO — the renderer in
//! apps/okra only draws what this model slices.
//!
//! Blocks: a turn is a divider line, then user text, assistant text
//! (appended per delta — the LAST line mutates, we never rescan), and
//! tool cards (started → finished). Scrolling: pinned mode tracks the
//! live edge; the moment the user scrolls up the TOP line index freezes,
//! so streaming appends never move a scrolled view until the user jumps
//! or scrolls back to the bottom.

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Divider,
    User,
    Assistant,
    Tool,
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct Scrollback {
    lines: Vec<Line>,
    /// Viewport's first line while UNPINNED (frozen against appends).
    top: usize,
    /// Pinned = follow the live edge (top derived per render).
    pinned: bool,
}

impl Scrollback {
    pub fn new() -> Self {
        Scrollback { lines: Vec::new(), top: 0, pinned: true }
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    pub fn pinned(&self) -> bool {
        self.pinned
    }

    fn push(&mut self, kind: LineKind, text: impl Into<String>) {
        self.lines.push(Line { kind, text: text.into() });
    }

    fn append_to_last(&mut self, kind: LineKind, text: &str) {
        if let Some(last) = self.lines.last_mut()
            && last.kind == kind
        {
            last.text.push_str(text);
        } else {
            self.push(kind, text);
        }
    }

    /// The top index a `height`-tall viewport would show right now.
    fn live_top(&self, height: usize) -> usize {
        self.lines.len().saturating_sub(height)
    }

    fn max_top(&self, height: usize) -> usize {
        self.live_top(height)
    }

    /// Scroll up by `n` lines (away from the live edge); unpin + freeze.
    pub fn scroll_up(&mut self, n: usize, height: usize) {
        if self.pinned {
            self.top = self.live_top(height);
            self.pinned = false;
        }
        self.top = self.top.saturating_sub(n);
    }

    /// Scroll down by `n` lines; pin again when the bottom is reached.
    pub fn scroll_down(&mut self, n: usize, height: usize) {
        if self.pinned {
            return;
        }
        self.top = (self.top + n).min(self.max_top(height));
        if self.top == self.max_top(height) {
            self.pinned = true;
        }
    }

    pub fn jump_to_bottom(&mut self) {
        self.top = 0;
        self.pinned = true;
    }

    /// The visible slice for a viewport `height` lines tall.
    pub fn viewport(&self, height: usize) -> &[Line] {
        if height == 0 || self.lines.is_empty() {
            return &[];
        }
        if self.pinned {
            let start = self.live_top(height);
            return &self.lines[start..];
        }
        let end = (self.top + height).min(self.lines.len());
        &self.lines[self.top..end]
    }

    /// Feed one NDJSON LoopEvent (tagged `event`, snake_case) into the
    /// model. Unknown events are ignored — the pager is a projection.
    pub fn feed(&mut self, event: &str, payload: &Value) {
        match event {
            "turn_started" => {
                let turn = payload["turn"].as_u64().unwrap_or(0);
                self.push(LineKind::Divider, format!("── turn {turn} "));
            }
            "user_message" => {
                let text = payload["text"].as_str().unwrap_or_default();
                self.push(LineKind::User, text);
            }
            "text_delta" => {
                let text = payload["text"].as_str().unwrap_or_default();
                self.append_to_last(LineKind::Assistant, text);
            }
            "tool_call_started" => {
                let name = payload["name"].as_str().unwrap_or("tool");
                self.push(LineKind::Tool, format!("▸ {name}"));
            }
            "tool_call_finished" => {
                let name = payload["name"].as_str().unwrap_or("tool");
                let is_error = payload["is_error"].as_bool().unwrap_or(false);
                let output = payload["output"].as_str().unwrap_or_default();
                let first_line = output.lines().next().unwrap_or("").chars().take(120).collect::<String>();
                let mark = if is_error { "✗" } else { "✓" };
                self.push(LineKind::Tool, format!("{mark} {name}: {first_line}"));
            }
            "steering_injected" => {
                let text = payload["text"].as_str().unwrap_or_default();
                self.push(LineKind::User, format!("[steered] {text}"));
            }
            "compaction_notice" => {
                let note = payload["note"].as_str().unwrap_or_default();
                self.push(LineKind::Note, format!("· {note}"));
            }
            "error" => {
                let message = payload["message"].as_str().unwrap_or_default();
                self.push(LineKind::Note, format!("! {message}"));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn feed(sb: &mut Scrollback, event: &str, payload: Value) {
        sb.feed(event, &payload);
    }

    #[test]
    fn streams_a_turn_into_blocks() {
        let mut sb = Scrollback::new();
        feed(&mut sb, "turn_started", json!({ "turn": 3 }));
        feed(&mut sb, "text_delta", json!({ "text": "hello " }));
        feed(&mut sb, "text_delta", json!({ "text": "world" }));
        feed(&mut sb, "tool_call_started", json!({ "id": "c1", "name": "read_file" }));
        feed(&mut sb, "tool_call_finished", json!({ "id": "c1", "name": "read_file", "is_error": false, "output": "line one\nline two" }));

        let texts: Vec<&str> = sb.lines().iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "── turn 3 ",
                "hello world",           // deltas COALESCED into one line
                "▸ read_file",
                "✓ read_file: line one", // first output line only
            ]
        );
        assert_eq!(sb.lines()[0].kind, LineKind::Divider);
        assert_eq!(sb.lines()[1].kind, LineKind::Assistant);
    }

    #[test]
    fn viewport_slices_from_the_live_edge_and_clamps() {
        let mut sb = Scrollback::new();
        for i in 0..10 {
            feed(&mut sb, "tool_call_started", json!({ "name": format!("t{i}") }));
        }
        // pinned: bottom 4 of 10
        let view: Vec<&str> = sb.viewport(4).iter().map(|l| l.text.as_str()).collect();
        assert_eq!(view, vec!["▸ t6", "▸ t7", "▸ t8", "▸ t9"]);

        sb.scroll_up(2, 4);
        assert!(!sb.pinned(), "scrolling up unpins");
        let view: Vec<&str> = sb.viewport(4).iter().map(|l| l.text.as_str()).collect();
        assert_eq!(view, vec!["▸ t4", "▸ t5", "▸ t6", "▸ t7"]);

        // clamped at the top
        sb.scroll_up(100, 4);
        let view: Vec<&str> = sb.viewport(4).iter().map(|l| l.text.as_str()).collect();
        assert_eq!(view, vec!["▸ t0", "▸ t1", "▸ t2", "▸ t3"]);

        sb.jump_to_bottom();
        assert!(sb.pinned());
        assert_eq!(sb.viewport(4).last().map(|l| l.text.as_str()), Some("▸ t9"));
    }

    #[test]
    fn streaming_while_scrolled_up_does_not_jump() {
        let mut sb = Scrollback::new();
        feed(&mut sb, "tool_call_started", json!({ "name": "a" }));
        feed(&mut sb, "tool_call_started", json!({ "name": "b" }));
        sb.scroll_up(1, 1); // freeze on line "a"
        feed(&mut sb, "tool_call_started", json!({ "name": "c" }));
        let view: Vec<&str> = sb.viewport(1).iter().map(|l| l.text.as_str()).collect();
        assert_eq!(view, vec!["▸ a"], "appends do not move a scrolled view");
        sb.scroll_down(2, 1); // back to the bottom → pinned
        assert!(sb.pinned());
        assert_eq!(sb.viewport(1).last().map(|l| l.text.as_str()), Some("▸ c"));
    }
}

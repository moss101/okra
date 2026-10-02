//! XML tool-call recovery (MASTER-PLAN §3 #37, qwen
//! `xml-tool-call-fallback`): some models emit tool calls as text in the
//! response body instead of the structured tool-call field. The turn loop
//! runs this recovery over every text-only response; recovered calls are
//! REAL calls (they execute through the same approved-bytes pipeline) and
//! the recovered blocks are stripped from the model-visible text so the
//! next sample is not confused by them.

use crate::messages::ToolCall;

/// Recover tool calls from `<tool_call>...</tool_call>` blocks. Supported
/// shapes (both common in open-weight model outputs):
/// - JSON body: `<tool_call>{"name":"read_file","arguments":{"path":"a"}}</tool_call>`
///   (`arguments` may also be spelled `parameters`, or the body may be
///   `{"name": ..., "args": ...}`);
/// - attributed form: `<tool_call name="read_file">{"path":"a"}</tool_call>`.
///
/// Returns the text with the recovered blocks REMOVED plus the calls in
/// order of appearance. Malformed blocks stay in the text (the model can
/// see its own mistake) and produce no call.
pub fn recover_xml_tool_calls(text: &str) -> (String, Vec<ToolCall>) {
    let mut calls = Vec::new();
    let mut cleaned = String::new();
    let mut rest = text;
    let mut seq: usize = 0;
    while let Some(start) = rest.find("<tool_call") {
        // keep everything before the block
        cleaned.push_str(&rest[..start]);
        let after_open = &rest[start..];
        let Some(open_end_rel) = after_open.find('>') else { break };
        let open_tag = &after_open[..=open_end_rel];
        let body_start = start + open_end_rel + 1;
        let Some(close_rel) = rest[body_start..].find("</tool_call>") else { break };
        let body = rest[body_start..body_start + close_rel].trim();
        let after_block = body_start + close_rel + "</tool_call>".len();

        match parse_call(open_tag, body, &mut seq) {
            Some(call) => calls.push(call),
            // malformed: keep the block verbatim — honesty over prettiness
            None => cleaned.push_str(&rest[start..after_block]),
        }
        rest = &rest[after_block..];
    }
    cleaned.push_str(rest);
    (cleaned, calls)
}

/// Parse one block from its open tag + body.
fn parse_call(open_tag: &str, body: &str, seq: &mut usize) -> Option<ToolCall> {
    /// `name="x"` (or `name='x'`) out of the open tag, if any.
    fn attr_name(open_tag: &str) -> Option<String> {
        let idx = open_tag.find("name=")?;
        let rest = open_tag[idx + "name=".len()..].trim_start();
        let q = rest.chars().next()?;
        if q == '"' || q == '\'' {
            let end = rest[1..].find(q)?;
            Some(rest[1..1 + end].to_string())
        } else {
            // unquoted attribute value ends at whitespace or `>`
            let end = rest.find(|c: char| c.is_whitespace() || c == '>')?;
            Some(rest[..end].to_string())
        }
    }

    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    if let Some(v) = parsed {
        let name = v
            .get("name")
            .and_then(|n| n.as_str())
            .map(str::to_string)
            .or_else(|| attr_name(open_tag))?;
        let args = v
            .get("arguments")
            .or_else(|| v.get("parameters"))
            .or_else(|| v.get("args"))
            .cloned()
            .unwrap_or(serde_json::Value::Object(Default::default()));
        let args_json = if args.is_string() {
            args.as_str().map(str::to_string)?
        } else {
            serde_json::to_string(&args).ok()?
        };
        *seq += 1;
        return Some(ToolCall {
            id: format!("recovered-{}", *seq),
            name,
            args_json,
        });
    }

    // non-JSON body with a name attribute: treat the body as a single
    // `input` string argument (honoring the donor's tolerance for loose
    // models without inventing a schema)
    if let Some(name) = attr_name(open_tag) {
        let args = serde_json::json!({ "input": body });
        *seq += 1;
        return Some(ToolCall {
            id: format!("recovered-{}", *seq),
            name,
            args_json: serde_json::to_string(&args).ok()?,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_body_recovers_name_and_arguments() {
        let text = "Before.\n<tool_call>{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.txt\"}}</tool_call>\nAfter.";
        let (cleaned, calls) = recover_xml_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].args_json, r#"{"path":"a.txt"}"#);
        assert!(calls[0].id.starts_with("recovered-"));
        assert_eq!(cleaned, "Before.\n\nAfter.", "block stripped from the text");
    }

    #[test]
    fn attributed_form_and_parameters_spelling() {
        let text = "<tool_call name=\"write_file\">{\"parameters\":{\"path\":\"b.txt\",\"content\":\"x\"}}</tool_call>";
        let (_, calls) = recover_xml_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "write_file");
        assert!(calls[0].args_json.contains("b.txt"));
    }

    #[test]
    fn multiple_blocks_recover_in_order() {
        let text = "<tool_call>{\"name\":\"a\",\"arguments\":{}}</tool_call>mid<tool_call>{\"name\":\"b\",\"arguments\":{}}</tool_call>";
        let (_, calls) = recover_xml_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "a");
        assert_eq!(calls[1].name, "b");
        assert_ne!(calls[0].id, calls[1].id);
    }

    #[test]
    fn malformed_blocks_stay_in_text_and_produce_nothing() {
        let text = "<tool_call>{not json}</tool_call>";
        let (cleaned, calls) = recover_xml_tool_calls(text);
        assert!(calls.is_empty());
        assert_eq!(cleaned, text, "the model sees its own malformed block");
    }

    #[test]
    fn unterminated_block_is_left_alone() {
        let text = "<tool_call>{\"name\":\"a\"}";
        let (cleaned, calls) = recover_xml_tool_calls(text);
        assert!(calls.is_empty());
        assert_eq!(cleaned, text);
    }

    #[test]
    fn plain_text_is_untouched() {
        let text = "no tool calls here, just <b>html-ish</b> text";
        let (cleaned, calls) = recover_xml_tool_calls(text);
        assert!(calls.is_empty());
        assert_eq!(cleaned, text);
    }
}

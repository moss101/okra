//! Windows restricted-token child spawn (the g5 unlock, scoped in
//! docs/m6-windows-port.md). Phase 1 — the cross-platform pieces:
//!
//! - [`build_env_block`]: the `CreateProcessAsUserW` environment block
//!   (wide, `key=value`, sorted, double-NUL terminated) — pure logic,
//!   unit-tested on every OS;
//! - [`quote_cmdline`]: argv re-quoted into the single mutable wide
//!   command line `CreateProcessAsUserW` parses (std::process::Command
//!   cannot drive it — the token API needs the raw call).
//!
//! Phase 2 (unsafe, windows-only, next): `OpenProcessToken` →
//! `CreateRestrictedToken(DISABLE_MAX_PRIVILEGE)` → `CreateProcessAsUserW`
//! with pipe inheritance via STARTUPINFOW. Verified signatures are in
//! docs/m6-windows-port.md. Nothing here executes on unix.

/// Build a `CreateProcessAsUserW` environment block from sorted pairs:
/// each `key=value` as UTF-16 with a NUL, the whole block ending in a
/// second NUL. Keys are sorted (Windows treats env blocks as sorted;
/// `GetEnvironmentStrings` returns them sorted and some children rely on
/// it). A key without `=` can never appear (callers pass pairs).
pub fn build_env_block(pairs: &[(String, String)]) -> Vec<u16> {
    let mut sorted: Vec<&(String, String)> = pairs.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in sorted {
        for unit in format!("{k}={v}").encode_utf16() {
            block.push(unit);
        }
        block.push(0);
    }
    block.push(0);
    block
}

/// Quote one argv element for a Windows command line: wrap in quotes when
/// the element contains a space, quote, or tab; double any embedded
/// quotes (the CommandLineToArgvW convention `CreateProcessAsUserW`
/// parses).
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty()
        && !arg
            .chars()
            .any(|c| c == ' ' || c == '\t' || c == '"' || c == '\n')
    {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        if c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// argv → one command line (space-joined, quoted per [`quote_arg`]).
pub fn build_command_line(argv: &[String]) -> String {
    argv.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_block_is_sorted_utf16_double_nul() {
        let block = build_env_block(&[
            ("ZLAST".into(), "1".into()),
            ("AFIRST".into(), "one".into()),
        ]);
        let as_string: String = String::from_utf16(&block).unwrap();
        // sorted: AFIRST first
        assert!(as_string.starts_with("AFIRST=one\u{0}ZLAST=1\u{0}\u{0}"));
    }

    #[test]
    fn empty_pairs_produce_the_terminator_only() {
        // an empty list is a single NUL (the list terminator); a non-empty
        // list ends with its own entry NUL + the terminator
        assert_eq!(build_env_block(&[]), vec![0]);
        let one = build_env_block(&[("K".into(), "v".into())]);
        assert_eq!(one, vec!['K' as u16, '=' as u16, 'v' as u16, 0, 0]);
    }

    #[test]
    fn quoting_wraps_spaces_and_escapes_quotes() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg("has space"), "\"has space\"");
        assert_eq!(quote_arg("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(quote_arg(""), "\"\"");
    }

    #[test]
    fn command_line_joins_quoted_elements() {
        let line = build_command_line(&[
            "C:\\bin\\okra.exe".into(),
            "run-subagent".into(),
            "--task".into(),
            "two words".into(),
        ]);
        assert_eq!(
            line,
            "C:\\bin\\okra.exe run-subagent --task \"two words\""
        );
    }
}

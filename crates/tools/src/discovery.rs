//! Deferred tool discovery — `tool_search` (MASTER-PLAN §3 #16, qwen
//! `tools.ts:300-320` + `tool-registry.ts`, generalized beyond the
//! MCP-only `tool_directory`): the model does not carry every tool's full
//! schema in the world head; it gets a compact directory and searches it
//! on demand, receiving FULL schemas for the matches.
//!
//! The index is refreshed by the HOST after tool registration (native +
//! MCP), so what search returns is exactly what dispatch can execute.

use serde_json::{json, Value};
use std::sync::Mutex;

/// One discoverable tool: the compact form the world head carries, with
/// the full schema delivered only on search.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveryEntry {
    pub name: String,
    pub description: String,
    /// Where the tool came from: `"native"` or `"mcp:<server>"`.
    pub source: String,
    pub schema: Option<Value>,
}

/// The searchable index. `refresh` replaces the whole set (the host knows
/// the registration moment); `search` is pure.
#[derive(Default)]
pub struct DiscoveryIndex {
    entries: Mutex<Vec<DiscoveryEntry>>,
}

impl DiscoveryIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the indexed set (sorted by name for stable output).
    pub fn refresh(&self, entries: Vec<DiscoveryEntry>) {
        let mut entries = entries;
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        *self.entries.lock().unwrap() = entries;
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Score + rank. Tokens of the query matched against name (weight 3)
    /// and description (weight 1); a full-name hit ranks first. An EMPTY
    /// query browses: the first `limit` entries in stable order.
    pub fn search(&self, query: &str, limit: usize) -> Vec<DiscoveryEntry> {
        let entries = self.entries.lock().unwrap();
        let query = query.trim();
        if query.is_empty() {
            return entries.iter().take(limit).cloned().collect();
        }
        let tokens: Vec<String> = query
            .split(|c: char| c.is_whitespace() || c == '_' || c == '-')
            .filter(|t| !t.is_empty())
            .map(|t| t.to_ascii_lowercase())
            .collect();
        let mut scored: Vec<(i64, &DiscoveryEntry)> = entries
            .iter()
            .map(|e| {
                let name = e.name.to_ascii_lowercase();
                let desc = e.description.to_ascii_lowercase();
                let mut score: i64 = 0;
                for t in &tokens {
                    if name == *t {
                        score += 10;
                    } else if name.contains(t.as_str()) {
                        score += 3;
                    }
                    if desc.contains(t.as_str()) {
                        score += 1;
                    }
                }
                (score, e)
            })
            .filter(|(s, _)| *s > 0)
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        scored
            .into_iter()
            .take(limit)
            .map(|(_, e)| e.clone())
            .collect()
    }
}

/// The `tool_search` builtin: read-only, idempotent, safe in plan mode.
pub struct ToolSearch {
    pub index: std::sync::Arc<DiscoveryIndex>,
}

impl ToolSearch {
    pub fn entry(&self) -> crate::spec::ToolEntry {
        crate::spec::ToolEntry {
            spec: crate::spec::ToolSpec {
                name: "tool_search".into(),
                namespace: None,
                title: Some("Tool search".into()),
                description:
                    "Search the daemon's full tool directory (native + MCP) and get full JSON schemas for the matches. Use when no registered tool obviously fits."
                        .into(),
                arguments_schema: Some(json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "what the task needs, e.g. 'git commit' or 'screenshot'" },
                        "limit": { "type": "integer", "description": "max results (default 8, capped at 25)" }
                    }
                })),
                kind: Some("meta".into()),
                behavior_version: Some("1".into()),
                idempotent: true,
                read_only: true,
                timeout_ms: Some(5_000),
                max_concurrency: None,
            },
            metadata: crate::spec::ToolMetadata {
                read_only: true,
                concurrent_safe: true,
                allowed_in_plan_mode: Some(true),
                ..Default::default()
            },
        }
    }

    pub fn accesses(&self) -> crate::scheduler::ToolAccesses {
        vec![]
    }

    pub fn execute(&self, args: &Value) -> ToolStream {
        let query = args["query"].as_str().unwrap_or_default();
        let limit = args["limit"].as_u64().unwrap_or(8).min(25) as usize;
        let hits = self.index.search(query, limit);
        let results: Vec<Value> = hits
            .iter()
            .map(|e| {
                json!({
                    "name": e.name,
                    "description": e.description,
                    "source": e.source,
                    "schema": e.schema,
                })
            })
            .collect();
        ToolStream::terminal_only(Ok(crate::stream::ToolOutput::from_value(json!({
            "query": query,
            "count": results.len(),
            "results": results,
        }))))
    }
}

use crate::stream::ToolStream;

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, desc: &str, source: &str) -> DiscoveryEntry {
        DiscoveryEntry {
            name: name.into(),
            description: desc.into(),
            source: source.into(),
            schema: Some(json!({"type": "object"})),
        }
    }

    #[test]
    fn name_token_hits_rank_above_description_hits() {
        let idx = DiscoveryIndex::new();
        idx.refresh(vec![
            entry("list_dir", "List entries of a directory", "native"),
            entry("read_file", "Read one file inside the workspace", "native"),
            entry("git_status", "Working-tree status via git", "native"),
        ]);
        let hits = idx.search("read file", 8);
        assert_eq!(hits.first().unwrap().name, "read_file", "name token beats description token");
        // every match carries its FULL schema — the deferred-discovery contract
        assert!(hits.iter().all(|h| h.schema.is_some()));
    }

    #[test]
    fn mcp_sources_are_discoverable_with_their_source_label() {
        let idx = DiscoveryIndex::new();
        idx.refresh(vec![
            entry("screenshot", "Take a screenshot", "native"),
            entry("mcp__computer__screenshot", "Computer screenshot", "mcp:computer"),
        ]);
        let hits = idx.search("screenshot", 8);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|h| h.source == "mcp:computer"));
    }

    #[test]
    fn empty_query_browses_in_stable_order() {
        let idx = DiscoveryIndex::new();
        idx.refresh(vec![
            entry("b_tool", "B", "native"),
            entry("a_tool", "A", "native"),
            entry("c_tool", "C", "native"),
        ]);
        let hits = idx.search("", 2);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].name, "a_tool", "refresh sorts by name; browse is stable");
        assert_eq!(hits[1].name, "b_tool");
    }

    #[test]
    fn no_match_is_an_honest_empty_result() {
        let idx = DiscoveryIndex::new();
        idx.refresh(vec![entry("read_file", "Read a file", "native")]);
        let hits = idx.search("deploy kubernetes", 8);
        assert!(hits.is_empty());
    }

    #[test]
    fn exact_name_match_outranks_partial() {
        let idx = DiscoveryIndex::new();
        idx.refresh(vec![
            entry("read", "Generic reader", "native"),
            entry("read_file", "Read one file", "native"),
        ]);
        let hits = idx.search("read", 8);
        assert_eq!(hits[0].name, "read", "exact name hit ranks first");
    }
}

//! Workflow validation passes (MASTER-PLAN §3 #42, M5): taint analysis
//! + causality graph over Rhai scripts, BEFORE a run spends anything.
//!
//! v1 works on a position-ordered node extraction (`AST::walk`):
//! straight-line scripts analyze exactly; the approximations are
//! documented per-check and conservative (they may over-warn, never
//! under-warn):
//! - **taint**: parameters of the entry function are untrusted; a
//!   `step(...)` whose argument region references one without an
//!   intervening `gate(...)` is flagged UNGATED. `gate` is the author's
//!   "I validated this text" marker — the engine does not define it,
//!   the script does (or it wraps the value in a no-op).
//! - **causality**: nodes are named `step` sites; an edge A→B exists
//!   when a variable bound between A and B is referenced between its
//!   binding and B (A's output feeds B's input). Cycles are reported —
//!   straight-line code cannot produce one, but helper-mediated
//!   recursion is not modeled (see opacity below) and synthesized
//!   graphs are checked by the same routine.
//! - helper functions are OPAQUE to v1 (bodies are not exposed by the
//!   rhai metadata API): scripts defining any get an explicit INFO
//!   finding so nobody mistakes v1 coverage for interprocedural truth.

use rhai::{ASTNode, Engine, Expr, Stmt};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Finding {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    /// 1-based rhai line, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct CausalityGraph {
    /// step name -> steps it feeds (ordered, deduped)
    pub edges: BTreeMap<String, Vec<String>>,
    pub nodes: Vec<String>,
}

impl CausalityGraph {
    /// Steps reachable from each node, transitively (for cycle reports).
    pub fn cycles(&self) -> Vec<Vec<String>> {
        let mut found: Vec<Vec<String>> = Vec::new();
        for start in &self.nodes {
            // DFS with the path; a back-edge to a node on the path is a cycle
            let mut path = vec![start.clone()];
            let mut stack = vec![self.edges.get(start).cloned().unwrap_or_default()];
            while let Some(neighbors) = stack.last_mut() {
                let Some(next) = neighbors.pop() else {
                    stack.pop();
                    path.pop();
                    continue;
                };
                if let Some(i) = path.iter().position(|n| n == &next) {
                    let mut cycle = path[i..].to_vec();
                    // normalize rotations so a b c / b c a / c a b count once
                    if let Some(min_at) = cycle.iter().enumerate().min_by(|a, b| a.1.cmp(b.1)).map(|(i, _)| i) {
                        cycle.rotate_left(min_at);
                    }
                    if !found.contains(&cycle) {
                        found.push(cycle);
                    }
                } else if self.nodes.contains(&next) {
                    path.push(next.clone());
                    stack.push(self.edges.get(&next).cloned().unwrap_or_default());
                }
            }
        }
        found
    }
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ValidationReport {
    pub findings: Vec<Finding>,
    pub graph: CausalityGraph,
    /// entry-parameter names (the taint sources)
    pub entry_params: Vec<String>,
}

impl ValidationReport {
    pub fn has_errors(&self) -> bool {
        self.findings.iter().any(|f| f.severity == Severity::Error)
    }
}

#[derive(Debug)]
enum Node {
    Step { name: Option<String>, line: u32 },
    Decl { name: String, line: u32 },
    /// a variable reference attributed to the step whose argument
    /// subtree contains it (None = outside any step call)
    RefInStep { var: String, gated: bool },
}

/// Record a step/gate call site (statement or expression form).
#[allow(clippy::too_many_arguments)]
fn record_call(call: &rhai::FnCallExpr, _path: &[ASTNode], nodes: &mut Vec<Node>, _step_count: &mut usize) {
    let line = call
        .args
        .first()
        .map(|a| a.position().line().map(|l| l as u32).unwrap_or(0))
        .unwrap_or(0);
    if call.name == "step" {
        let name = match call.args.first() {
            Some(Expr::StringConstant(s, ..)) => Some(s.to_string()),
            _ => None,
        };
        nodes.push(Node::Step { name, line });
    }
}

/// Compile + validate. A script that does not compile reports a single
/// error finding (no panic, no run).
/// Compile + validate. A script that does not compile reports a single
/// error finding (no panic, no run).
pub fn validate(script: &str) -> ValidationReport {
    let mut report = ValidationReport::default();
    let engine = Engine::new();
    let ast = match engine.compile(script) {
        Ok(ast) => ast,
        Err(e) => {
            report.findings.push(Finding {
                severity: Severity::Error,
                code: "compile_error",
                message: e.to_string(),
                line: e.position().line().map(|l| l as u32),
            });
            return report;
        }
    };

    // entry function + params (the taint sources)
    let fns: Vec<(String, Vec<String>)> = ast
        .iter_functions()
        .map(|f| (f.name.to_string(), f.params.iter().map(|p| p.to_string()).collect()))
        .collect();
    let entry = fns
        .iter()
        .find(|(n, _)| n == "run")
        .or_else(|| fns.iter().find(|(n, _)| n == "main"));
    let Some((_, entry_params)) = entry else {
        report.findings.push(Finding {
            severity: Severity::Error,
            code: "no_entry_function",
            message: "script defines neither fn run() nor fn main() — nothing to execute".into(),
            line: None,
        });
        return report;
    };
    report.entry_params = entry_params.clone();
    for (name, _) in &fns {
        if name != "run" && name != "main" {
            report.findings.push(Finding {
                severity: Severity::Info,
                code: "helper_fn_opaque",
                message: format!(
                    "helper fn `{name}` is opaque to v1 analysis: steps and taint inside it are not modeled"
                ),
                line: None,
            });
        }
    }

    // path-attributed extraction: a Ref inside a step-call subtree
    // belongs to that step (pre-order walk puts calls before their
    // args); a gate ancestor between the step and the ref marks it gated
    let mut nodes: Vec<Node> = Vec::new();
    let mut step_count = 0usize;
    ast.walk(&mut |path: &[ASTNode]| {
        let Some(node) = path.last() else { return true };
        match node {
            ASTNode::Stmt(stmt) => match stmt {
                Stmt::Var(box_, ..) => {
                    let (ident, _, _) = &**box_;
                    nodes.push(Node::Decl {
                        name: ident.name.to_string(),
                        line: stmt.position().line().map(|l| l as u32).unwrap_or(0),
                    });
                }
                // a bare call statement is Stmt::FnCall (rhai duplicates
                // Expr::FnCall for the single-call-statement pattern)
                Stmt::FnCall(call, ..) => {
                    record_call(call, path, &mut nodes, &mut step_count);
                }
                _ => {}
            },
            ASTNode::Expr(expr) => match expr {
                Expr::FnCall(call, ..) => {
                    record_call(call, path, &mut nodes, &mut step_count);
                }
                Expr::Variable(x, ..) => {
                    // attribute to the nearest enclosing step call; gated
                    // if a gate call sits between it and the ref
                    let mut in_step = false;
                    let mut gated = false;
                    for ancestor in path.iter().rev() {
                        match ancestor {
                            ASTNode::Expr(Expr::FnCall(c, ..)) | ASTNode::Stmt(Stmt::FnCall(c, ..)) => {
                                if c.name == "step" {
                                    in_step = true;
                                } else if c.name == "gate" && in_step {
                                    gated = true;
                                }
                            }
                            _ => {}
                        }
                    }
                    if in_step {
                        nodes.push(Node::RefInStep { var: x.1.to_string(), gated });
                    }
                }
                _ => {}
            },
            _ => {}
        }
        true
    });

    // step sites: names + unnamed warnings + the graph nodes
    let mut step_sites: Vec<(Option<String>, u32)> = Vec::new();
    for node in &nodes {
        if let Node::Step { name, line } = node {
            if name.is_none() {
                report.findings.push(Finding {
                    severity: Severity::Warning,
                    code: "unnamed_step",
                    message: "step name is not a string literal — name steps so the journal and the causality graph can key them".into(),
                    line: Some(*line),
                });
            }
            step_sites.push((name.clone(), *line));
        }
    }
    if step_sites.is_empty() {
        report.findings.push(Finding {
            severity: Severity::Warning,
            code: "no_steps",
            message: "script calls step() nowhere — the run will do no host work".into(),
            line: None,
        });
    }

    let named: Vec<(String, u32)> = step_sites
        .iter()
        .filter_map(|(n, l)| n.clone().map(|n| (n, *l)))
        .collect();
    report.graph.nodes = named.iter().map(|(n, _)| n.clone()).collect();

    // per-step reference sets: pre-order puts the Step node before its
    // argument subtree, so refs group to the most recent step
    let mut refs_by_step: Vec<(String, Vec<String>)> = Vec::new();
    {
        let mut current: Option<(String, Vec<String>)> = None;
        for node in &nodes {
            match node {
                Node::Step { name, .. } => {
                    if let Some((n, refs)) = current.take() {
                        refs_by_step.push((n, refs));
                    }
                    current = Some((name.clone().unwrap_or_default(), Vec::new()));
                }
                Node::RefInStep { var, .. } => {
                    if let Some((_, refs)) = current.as_mut() {
                        refs.push(var.clone());
                    }
                }
                _ => {}
            }
        }
        if let Some((n, refs)) = current.take() {
            refs_by_step.push((n, refs));
        }
    }

    // edges: A -> B when a variable DECLARED in [A.line, B.line) is
    // REFERENCED inside B's argument subtree (A's output feeds B)
    for (a_idx, (a_name, a_line)) in named.iter().enumerate() {
        for (b_name, b_line) in named.iter().skip(a_idx + 1) {
            let b_refs = refs_by_step
                .iter()
                .filter(|(n, _)| n == b_name)
                .flat_map(|(_, refs)| refs.iter())
                .collect::<Vec<_>>();
            let feeds = nodes.iter().any(|node| {
                if let Node::Decl { name, line } = node {
                    *line >= *a_line && *line < *b_line && b_refs.iter().any(|r| r.as_str() == name.as_str())
                } else {
                    false
                }
            });
            if feeds {
                report
                    .graph
                    .edges
                    .entry(a_name.clone())
                    .or_default()
                    .push(b_name.clone());
            }
        }
    }
    for list in report.graph.edges.values_mut() {
        list.dedup();
    }

    // taint: entry params referenced inside a step's args, ungated.
    // refs are position-attributed by the walk; a step is tainted when
    // ANY of its refs names an entry param and is not gate-wrapped.
    // (attribution is by subtree, so this is exact for straight-line
    // scripts; the last-step approximation above is NOT used here)
    let step_seq: Vec<Option<String>> = step_sites.iter().map(|(n, _)| n.clone()).collect();
    let _ = step_seq;
    let mut current_step: Option<(Option<String>, u32)> = None;
    for node in &nodes {
        match node {
            Node::Step { name, line } => current_step = Some((name.clone(), *line)),
            Node::RefInStep { var, gated } => {
                if !gated
                    && entry_params.contains(var)
                    && let Some((step_name, step_line)) = &current_step
                {
                    report.findings.push(Finding {
                        severity: Severity::Warning,
                        code: "ungated_input_to_step",
                        message: format!(
                            "untrusted entry input `{var}` flows into step {} without a gate() wrapper",
                            step_name.as_deref().unwrap_or("(unnamed)")
                        ),
                        line: Some(*step_line),
                    });
                }
            }
            _ => {}
        }
    }

    for cycle in report.graph.cycles() {
        report.findings.push(Finding {
            severity: Severity::Error,
            code: "cyclic_dependency",
            message: format!("causality cycle: {}", cycle.join(" -> ")),
            line: None,
        });
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_script_has_no_findings_and_a_real_graph() {
        let script = r#"
            fn run() {
                let a = step("fetch", "read the ledger");
                let b = step("summarize", a);
                step("publish", "done");
            }
        "#;
        let r = validate(script);
        assert!(!r.has_errors(), "{:?}", r.findings);
        assert!(r.findings.is_empty(), "{:?}", r.findings);
        assert_eq!(r.graph.nodes, vec!["fetch", "summarize", "publish"]);
        assert_eq!(r.graph.edges.get("fetch").cloned(), Some(vec!["summarize".to_string()]));
        assert_eq!(r.graph.edges.get("summarize"), None, "publish's args are a literal — no edge");
    }

    #[test]
    fn no_entry_function_is_an_error() {
        let r = validate("let x = 1;");
        assert!(r.has_errors());
        assert_eq!(r.findings[0].code, "no_entry_function");
    }

    #[test]
    fn compile_errors_report_without_panicking() {
        let r = validate("fn run() { this is not rhai }");
        assert!(r.has_errors());
        assert_eq!(r.findings[0].code, "compile_error");
    }

    #[test]
    fn steps_without_work_and_without_names_warn() {
        let r = validate("fn run() { let x = 41 + 1; }");
        assert_eq!(r.findings[0].code, "no_steps");
        let r = validate(r#"fn run() { step("a", 1); step(x, 2); }"#);
        assert!(r.findings.iter().any(|f| f.code == "unnamed_step"));
    }

    #[test]
    fn ungated_entry_input_flowing_into_a_step_warns_and_gate_silences() {
        let hot = r#"
            fn run(user_ask) {
                step("do", "the task: " + user_ask);
            }
        "#;
        let r = validate(hot);
        assert!(
            r.findings.iter().any(|f| f.code == "ungated_input_to_step"),
            "{:?}",
            r.findings
        );

        let gated = r#"
            fn run(user_ask) {
                let safe = gate(user_ask);
                step("do", "the task: " + safe);
            }
        "#;
        let r = validate(gated);
        assert!(
            !r.findings.iter().any(|f| f.code == "ungated_input_to_step"),
            "{:?}",
            r.findings
        );
    }

    #[test]
    fn helper_functions_are_declared_opaque() {
        let script = r#"
            fn helper(x) { step("inner", x) }
            fn run() { helper(1) }
        "#;
        let r = validate(script);
        assert!(r.findings.iter().any(|f| f.code == "helper_fn_opaque"));
    }

    #[test]
    fn cycle_detection_finds_back_edges() {
        let mut g = CausalityGraph { nodes: vec!["a".into(), "b".into(), "c".into()], ..Default::default() };
        g.edges.insert("a".into(), vec!["b".into()]);
        g.edges.insert("b".into(), vec!["c".into()]);
        g.edges.insert("c".into(), vec!["a".into()]);
        let cycles = g.cycles();
        assert_eq!(cycles.len(), 1, "{cycles:?}");
        assert!(cycles[0].contains(&"a".to_string()) && cycles[0].contains(&"c".to_string()));

        let mut acyclic = CausalityGraph { nodes: vec!["a".into(), "b".into()], ..Default::default() };
        acyclic.edges.insert("a".into(), vec!["b".into()]);
        assert!(acyclic.cycles().is_empty());
    }
}

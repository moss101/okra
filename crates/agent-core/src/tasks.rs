//! One task runtime with kinds (MASTER-PLAN §3 #39 — replaces qwen's 8
//! overlapping orchestration primitives with ONE store + kind discriminant).
//!
//! Kinds: todo | goal | job | scheduled | subagent | workflow_run.
//! `scheduled` tasks carry the automation self-mutation guard contract
//! (§3 #40, enforced at the host tool-policy layer).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Todo,
    Goal,
    Job,
    Scheduled,
    Subagent,
    WorkflowRun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// ZCode automation self-mutation guard (`automationToolPolicy.ts`):
/// Cron*/OffPeak* mutation tools are DENIED on automation/off-peak turns —
/// a scheduled task must not be able to reschedule itself.
pub const AUTOMATION_MUTATION_TOOLS: [&str; 4] =
    ["cron_create", "cron_update", "cron_delete", "offpeak_create"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationGuardDecision {
    Allowed,
    Denied,
}

/// The guard: is `tool` allowed on a turn driven by a `scheduled` task?
pub fn automation_mutation_allowed(turn_task_kind: Option<TaskKind>, tool: &str) -> AutomationGuardDecision {
    if turn_task_kind == Some(TaskKind::Scheduled)
        && AUTOMATION_MUTATION_TOOLS.contains(&tool)
    {
        AutomationGuardDecision::Denied
    } else {
        AutomationGuardDecision::Allowed
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    pub kind: TaskKind,
    pub title: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task: Option<String>,
    /// Kind-specific payload (todo text, cron spec, subagent grant, …).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The task store — one runtime, indexed by id and kind.
#[derive(Debug, Default)]
pub struct TaskStore {
    tasks: HashMap<String, Task>,
    order: Vec<String>,
}

impl TaskStore {
    pub fn create(&mut self, id: impl Into<String>, kind: TaskKind, title: impl Into<String>) -> &Task {
        let id = id.into();
        let task = Task {
            id: id.clone(),
            kind,
            title: title.into(),
            status: TaskStatus::Pending,
            parent_task: None,
            payload: Value::Null,
            error: None,
        };
        self.order.push(id.clone());
        self.tasks.insert(id.clone(), task);
        self.tasks.get(&id).expect("just inserted")
    }

    pub fn get(&self, id: &str) -> Option<&Task> {
        self.tasks.get(id)
    }

    pub fn set_status(&mut self, id: &str, status: TaskStatus) -> Result<(), String> {
        let task = self.tasks.get_mut(id).ok_or_else(|| format!("unknown task {id}"))?;
        task.status = status;
        Ok(())
    }

    pub fn set_payload(&mut self, id: &str, payload: Value) -> Result<(), String> {
        let task = self.tasks.get_mut(id).ok_or_else(|| format!("unknown task {id}"))?;
        task.payload = payload;
        Ok(())
    }

    /// Tasks in creation order (the todo list projection).
    pub fn list(&self) -> Vec<&Task> {
        self.order.iter().filter_map(|id| self.tasks.get(id)).collect()
    }

    pub fn list_kind(&self, kind: TaskKind) -> Vec<&Task> {
        self.list().into_iter().filter(|t| t.kind == kind).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_store_many_kinds() {
        let mut store = TaskStore::default();
        store.create("t1", TaskKind::Todo, "fix flake");
        store.create("g1", TaskKind::Goal, "ship M1");
        store.create("s1", TaskKind::Scheduled, "nightly bench");
        store.create("w1", TaskKind::WorkflowRun, "wf-42");
        assert_eq!(store.list().len(), 4);
        assert_eq!(store.list_kind(TaskKind::Todo).len(), 1);
        store.set_status("t1", TaskStatus::Completed).unwrap();
        assert_eq!(store.get("t1").unwrap().status, TaskStatus::Completed);
    }

    #[test]
    fn automation_guard_denies_self_mutation_on_scheduled_turns() {
        assert_eq!(
            automation_mutation_allowed(Some(TaskKind::Scheduled), "cron_create"),
            AutomationGuardDecision::Denied
        );
        assert_eq!(
            automation_mutation_allowed(Some(TaskKind::Scheduled), "cron_list"),
            AutomationGuardDecision::Allowed
        );
        // the same tools are fine on ordinary turns
        assert_eq!(
            automation_mutation_allowed(Some(TaskKind::Job), "cron_create"),
            AutomationGuardDecision::Allowed
        );
        assert_eq!(
            automation_mutation_allowed(None, "cron_delete"),
            AutomationGuardDecision::Allowed
        );
    }
}

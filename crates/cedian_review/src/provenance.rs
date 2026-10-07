//! Provenance store: cedian-owned `AgentEdit` records (plan §17).
//!
//! Keyed by `task_id + tool_call_id`, snapshotted at `tool_execution_end`
//! time. Survives OMP compaction/session-restore — OMP must never delete or
//! rewrite these. Post-compaction backfill is impossible by design (compaction
//! keeps summaries/todos, drops verbatim turns + fine attribution), so any
//! hunk WITHOUT a correlating record is `UNATTRIBUTED`, never misattributed.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// One tracked agent edit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEdit {
    /// Correlating tool call id (`tool_execution_end.tool_call_id`).
    pub tool_call_id: String,
    /// Owning task.
    pub task_id: String,
    /// Buffer path edited.
    pub file: String,
    /// Buffer text before the edit.
    pub before: String,
    /// Buffer text after the edit.
    pub after: String,
    /// Unix millis when the edit completed.
    pub timestamp_ms: u64,
}

/// Provenance failures (caller-visible).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvenanceError {
    /// No record for this task + tool call (→ `UNATTRIBUTED`, not an error).
    Missing {
        task_id: String,
        tool_call_id: String,
    },
}

impl std::fmt::Display for ProvenanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing {
                task_id,
                tool_call_id,
            } => {
                write!(f, "no provenance for {task_id}/{tool_call_id}")
            }
        }
    }
}

impl std::error::Error for ProvenanceError {}

/// Cedian-owned edit journal. Append-only in practice (no delete API:
/// compaction must never remove records; only explicit task archive drops them).
#[derive(Debug, Default)]
pub struct ProvenanceStore {
    records: HashMap<(String, String), AgentEdit>,
}

impl ProvenanceStore {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot one completed edit (call at `tool_execution_end`).
    pub fn record(&mut self, edit: AgentEdit) {
        self.records
            .insert((edit.task_id.clone(), edit.tool_call_id.clone()), edit);
    }

    /// Look up the record for a task + tool call. `Missing` means the hunk is
    /// `UNATTRIBUTED` — visible and per-hunk reviewable, excluded from bulk
    /// accept, never re-attributed without a re-driven tracked edit.
    pub fn lookup(&self, task_id: &str, tool_call_id: &str) -> Result<&AgentEdit, ProvenanceError> {
        self.records
            .get(&(task_id.to_string(), tool_call_id.to_string()))
            .ok_or_else(|| ProvenanceError::Missing {
                task_id: task_id.to_string(),
                tool_call_id: tool_call_id.to_string(),
            })
    }

    /// All edits for one task (task history view).
    pub fn edits_for_task(&self, task_id: &str) -> Vec<&AgentEdit> {
        self.records
            .values()
            .filter(|e| e.task_id == task_id)
            .collect()
    }

    /// Drop a task's records (explicit task archive only).
    /// Every record, oldest first (stable order for persistence).
    pub fn all(&self) -> Vec<AgentEdit> {
        let mut out: Vec<AgentEdit> = self.records.values().cloned().collect();
        out.sort_by(|a, b| {
            (a.timestamp_ms, &a.task_id, &a.tool_call_id).cmp(&(
                b.timestamp_ms,
                &b.task_id,
                &b.tool_call_id,
            ))
        });
        out
    }

    /// Rebuild a store from persisted records (cedian-owned, survives OMP
    /// compaction — §17 R1).
    pub fn from_records(records: Vec<AgentEdit>) -> Self {
        let mut store = Self::new();
        for r in records {
            store.record(r);
        }
        store
    }

    pub fn drop_task(&mut self, task_id: &str) {
        self.records.retain(|(t, _), _| t != task_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit() -> AgentEdit {
        AgentEdit {
            tool_call_id: "c1".to_string(),
            task_id: "task-1".to_string(),
            file: "/a.rs".to_string(),
            before: "a".to_string(),
            after: "b".to_string(),
            timestamp_ms: 1,
        }
    }

    #[test]
    fn record_and_lookup() {
        let mut s = ProvenanceStore::new();
        s.record(edit());
        assert_eq!(s.lookup("task-1", "c1").unwrap().after, "b");
        assert!(matches!(
            s.lookup("task-1", "zzz"),
            Err(ProvenanceError::Missing { .. })
        ));
    }

    #[test]
    fn drop_task_only() {
        let mut s = ProvenanceStore::new();
        s.record(edit());
        s.drop_task("task-1");
        assert!(s.edits_for_task("task-1").is_empty());
    }
}

//! Workflow persistence: `workflow.json` in a workspace's state dir
//! (ADR-0044), shared by the app and the CLI. A JSON round-trip of
//! `WorkflowState` in a `snapshot_version` envelope (ADR-0016 / P3): other
//! versions fail closed.

use cedian_workflow::WorkflowState;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// `workflow.json` schema version. Bump on any `WorkflowState` shape change.
pub const WORKFLOW_SNAPSHOT_VERSION: u32 = 2;

/// On-disk shape: `{"snapshot_version": N, ...WorkflowState}`.
#[derive(Serialize, Deserialize)]
struct Stored<S> {
    /// Missing in pre-P3 files → 0 → rejected.
    #[serde(default)]
    snapshot_version: u32,
    #[serde(flatten)]
    state: S,
}

fn workflow_path(state_dir: &Path) -> PathBuf {
    state_dir.join("workflow.json")
}

/// Whether a workflow was ever started here.
pub fn exists(state_dir: &Path) -> bool {
    workflow_path(state_dir).exists()
}

/// Load the workflow, or fail with usage hint when none is running.
pub fn load(state_dir: &Path) -> Result<WorkflowState, String> {
    let raw = std::fs::read_to_string(workflow_path(state_dir))
        .map_err(|_| "no workflow: run `cedian workflow run <kind> <title>` first".to_string())?;
    let version: Stored<serde::de::IgnoredAny> =
        serde_json::from_str(&raw).map_err(|e| format!("corrupt workflow.json: {e}"))?;
    if version.snapshot_version != WORKFLOW_SNAPSHOT_VERSION {
        return Err(format!(
            "workflow state too old (got v{}, want v{WORKFLOW_SNAPSHOT_VERSION}), \
             re-baseline: run `cedian workflow run <kind> <title>`",
            version.snapshot_version
        ));
    }
    let stored: Stored<WorkflowState> =
        serde_json::from_str(&raw).map_err(|e| format!("corrupt workflow.json: {e}"))?;
    Ok(stored.state)
}

/// Save the workflow into the workspace's state dir (ADR-0044).
pub fn save(state_dir: &Path, state: &WorkflowState) -> Result<(), String> {
    let raw = serde_json::to_string_pretty(&Stored {
        snapshot_version: WORKFLOW_SNAPSHOT_VERSION,
        state,
    })
    .map_err(|e| e.to_string())?;
    std::fs::write(workflow_path(state_dir), raw).map_err(|e| e.to_string())
}

/// `workflow.json` in a state dir as the workflow channel's store.
pub struct DiskWorkflowStore(pub PathBuf);

impl cedian_workflow::WorkflowStore for DiskWorkflowStore {
    fn load(&self) -> Result<Option<WorkflowState>, String> {
        if !exists(&self.0) {
            return Ok(None);
        }
        load(&self.0).map(Some)
    }
    fn save(&self, state: &WorkflowState) -> Result<(), String> {
        save(&self.0, state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cedian_workflow::{Complexity, Risk, TaskKind, TaskProfile};

    fn dir(tag: &str) -> crate::test_dir::TestDir {
        crate::test_dir::TestDir::new(&format!("wf-store-{tag}"))
    }

    #[test]
    fn roundtrip_carries_version() {
        let d = dir("ok");
        let state = WorkflowState::start(TaskProfile {
            title: "t".to_string(),
            kind: TaskKind::BugFix,
            complexity: Complexity::Small,
            risk: Risk::Low,
            surfaces: vec![],
            constraints: vec![],
            acceptance_criteria: vec![],
        })
        .unwrap();
        save(&d, &state).unwrap();
        let raw = std::fs::read_to_string(workflow_path(&d)).unwrap();
        assert!(raw.contains("\"snapshot_version\": 2"));
        assert_eq!(load(&d).unwrap().task.title, "t");
    }

    #[test]
    fn unversioned_or_other_version_fails_closed() {
        let d = dir("stale");
        std::fs::write(workflow_path(&d), "{}").unwrap();
        assert!(load(&d).unwrap_err().contains("too old (got v0"));
        // v1 = pre-ADR-0024 evidence (`ok: bool`, no code state).
        std::fs::write(workflow_path(&d), r#"{"snapshot_version":1}"#).unwrap();
        assert!(load(&d).unwrap_err().contains("too old (got v1"));
    }
}

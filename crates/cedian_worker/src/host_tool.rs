//! `cedian_worktree_request` (P5, ADR-0022 / ADR-0009): OMP asks, cedian
//! creates. Mechanism only — the same `spawn` + [`Registry`] row the
//! `cedian worker spawn` verb uses; OMP keeps the orchestration policy.

use crate::{Registry, WorkerError, WorkerStatus, spawn};
use omp_rpc::HostTool;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// Host tool name (listed in `cedian_workflow::CHANNEL_TOOLS`).
pub const WORKTREE_REQUEST_TOOL: &str = "cedian_worktree_request";

/// Create worktree `id` off `base` (default `HEAD`) and record it in the
/// registry under `state` (the repo's cedian state dir). The id is checked
/// against the registry BEFORE `git worktree add`, so a refused request
/// never leaves an unregistered tree behind.
pub fn request_worktree(
    repo: &Path,
    state: &Path,
    args: &Map<String, Value>,
) -> Result<String, String> {
    let text = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or("");
    let id = text("id");
    let title = text("title");
    if id.is_empty() || title.is_empty() {
        return Err("missing `id` or `title`".to_string());
    }
    let base = match text("base") {
        "" => "HEAD",
        b => b,
    };
    let (mut reg, _) = Registry::open(state).map_err(|e| e.to_string())?;
    if reg.get(id).is_some() {
        return Err(WorkerError::Exists(id.to_string()).to_string());
    }
    let mut head = spawn(repo, id, base).map_err(|e| e.to_string())?;
    head.status = WorkerStatus::Running;
    head.task_title = title.to_string();
    head.kind = text("kind").to_string();
    let reply = format!(
        "worktree {} at {} (branch {}, base {base})",
        head.id,
        repo.join(&head.worktree).display(),
        head.branch
    );
    reg.insert(head).map_err(|e| e.to_string())?;
    reg.save(state).map_err(|e| e.to_string())?;
    Ok(reply)
}

/// The host tool over `repo` (the repo root, like `cedian worker`).
pub fn worktree_request_tool(repo: PathBuf, state: PathBuf) -> HostTool {
    let params = json!({
        "type": "object",
        "properties": {
            "id": {"type": "string", "description": "worker id: [A-Za-z0-9._-]"},
            "title": {"type": "string"},
            "kind": {"type": "string"},
            "base": {"type": "string", "description": "branch or commit (default HEAD)"}
        },
        "required": ["id", "title"],
        "additionalProperties": false
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    HostTool::new(
        WORKTREE_REQUEST_TOOL,
        "cedian's own host tool (trusted). Ask the cedian IDE for an isolated git worktree for a \
         parallel worker; never run `git worktree add` yourself. Returns the worktree id and path.",
        params,
        move |args, _ctx| {
            request_worktree(&repo, &state, &args)
                .map(Into::into)
                .map_err(Into::into)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::tests::fixture;

    fn state(repo: &Path) -> PathBuf {
        repo.with_extension("state")
    }

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn request_creates_and_registers() {
        let repo = fixture();
        let out = request_worktree(
            &repo,
            &state(&repo),
            &args(json!({"id": "w1", "title": "try it"})),
        )
        .unwrap();
        assert!(out.contains("branch cedian-worker/w1"), "{out}");
        assert!(repo.join(".worktrees/w1/a.txt").exists());
        let (reg, existed) = Registry::open(&state(&repo)).unwrap();
        assert!(existed);
        let head = reg.get("w1").unwrap();
        assert_eq!(head.task_title, "try it");
        assert_eq!(head.status, WorkerStatus::Running);
    }

    #[test]
    fn duplicate_and_bad_ids_are_refused() {
        let repo = fixture();
        request_worktree(
            &repo,
            &state(&repo),
            &args(json!({"id": "w1", "title": "a"})),
        )
        .unwrap();
        let dup = request_worktree(
            &repo,
            &state(&repo),
            &args(json!({"id": "w1", "title": "b"})),
        )
        .unwrap_err();
        assert!(dup.contains("w1"), "{dup}");
        assert!(
            request_worktree(
                &repo,
                &state(&repo),
                &args(json!({"id": "../x", "title": "c"}))
            )
            .is_err()
        );
        assert!(!repo.join(".worktrees/x").exists());
        assert!(request_worktree(&repo, &state(&repo), &args(json!({"id": "w2"}))).is_err());
    }
}

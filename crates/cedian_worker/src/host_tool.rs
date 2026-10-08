//! `cedian_worktree_request` (P5, ADR-0022 / ADR-0009): OMP asks, cedian
//! creates. Mechanism only — the same `spawn` + [`Registry`] row the
//! `cedian worker spawn` verb uses; OMP keeps the orchestration policy.

use crate::worktree::discard;
use crate::{Registry, WorkerError, WorkerStatus, spawn, validate_id};
use omp_rpc::HostTool;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// Host tool name (listed in `cedian_workflow::CHANNEL_TOOLS`).
pub const WORKTREE_REQUEST_TOOL: &str = "cedian_worktree_request";

/// The ADR-0033 brief fields a request must carry, in the order an error
/// names them.
const BRIEF_FIELDS: [&str; 5] = ["goal", "scope.write", "acceptance", "verify", "timebox_min"];

/// Every required brief field that is missing or empty (ADR-0033 decision 1).
fn missing_brief_fields(args: &Map<String, Value>) -> Vec<&'static str> {
    let text = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    };
    let lines = |v: Option<&Value>| {
        v.and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty() && items.iter().all(|i| text(Some(i))))
    };
    BRIEF_FIELDS
        .into_iter()
        .filter(|field| {
            let present = match *field {
                "goal" => text(args.get("goal")),
                "scope.write" => lines(args.get("scope").and_then(|s| s.get("write"))),
                "timebox_min" => args
                    .get("timebox_min")
                    .and_then(Value::as_f64)
                    .is_some_and(|m| m > 0.0),
                other => lines(args.get(other)),
            };
            !present
        })
        .collect()
}

/// Create worktree `id` off `base` (default `HEAD`) for the brief in
/// `args`, record it in the registry under `state` (the repo's cedian state
/// dir) and store the brief beside it as revision 1. A request without a
/// whole brief, or whose id the registry already has, is refused BEFORE
/// `git worktree add`; the brief is stored before the tree is made, and a
/// later failure takes tree and brief back, so a failed request leaves
/// neither and every row has its brief (ADR-0033). The registry lock is
/// held throughout: OMP runs host tool calls in parallel.
pub fn request_worktree(
    repo: &Path,
    state: &Path,
    args: &Map<String, Value>,
) -> Result<String, String> {
    let text = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or("");
    let id = text("id");
    let mut missing = missing_brief_fields(args);
    if id.is_empty() {
        missing.insert(0, "id");
    }
    if !missing.is_empty() {
        return Err(format!(
            "no worktree: the brief is missing {} (ADR-0033)",
            missing.join(", ")
        ));
    }
    let base = match text("base") {
        "" => "HEAD",
        b => b,
    };
    let _lock = Registry::lock(state).map_err(|e| e.to_string())?;
    let (mut reg, _) = Registry::open(state).map_err(|e| e.to_string())?;
    if reg.get(id).is_some() {
        return Err(WorkerError::Exists(id.to_string()).to_string());
    }
    validate_id(id).map_err(|e| e.to_string())?;
    let briefs = state.join("briefs");
    let brief = briefs.join(format!("{id}.1.json"));
    std::fs::create_dir_all(&briefs)
        .and_then(|()| std::fs::write(&brief, Value::Object(args.clone()).to_string()))
        .map_err(|e| format!("brief: {e}"))?;
    let mut head = match spawn(repo, id, base) {
        Ok(head) => head,
        Err(e) => {
            let _ = std::fs::remove_file(&brief);
            return Err(e.to_string());
        }
    };
    head.status = WorkerStatus::Running;
    head.task_title = text("goal").to_string();
    head.kind = text("kind").to_string();
    let reply = format!(
        "worktree {} at {} (branch {}, base {base})",
        head.id,
        repo.join(&head.worktree).display(),
        head.branch
    );
    let saved = reg
        .insert(head.clone())
        .and_then(|()| reg.save(state))
        .map_err(|e| e.to_string());
    if let Err(e) = saved {
        let _ = std::fs::remove_file(&brief);
        return Err(match discard(repo, &head) {
            Ok(()) => e,
            Err(left) => format!("{e}; its tree was left behind: {left}"),
        });
    }
    Ok(reply)
}

/// The host tool over `repo` (the repo root, like `cedian worker`).
pub fn worktree_request_tool(repo: PathBuf, state: PathBuf) -> HostTool {
    let lines = |what: &str| json!({"type": "array", "items": {"type": "string"}, "minItems": 1, "description": what});
    let params = json!({
        "type": "object",
        "properties": {
            "id": {"type": "string", "description": "worker id: [A-Za-z0-9._-]"},
            "goal": {"type": "string", "description": "one sentence: what the worker is for"},
            "scope": {
                "type": "object",
                "properties": {
                    "write": lines("path globs the worker may write"),
                    "deny": lines("path globs it must not write")
                },
                "required": ["write"]
            },
            "acceptance": lines("checkable lines that say it is done"),
            "verify": lines("commands or verification-profile ids that check it"),
            "timebox_min": {"type": "number", "description": "minutes before it shows as stuck"},
            "context": lines("file and PR pointers"),
            "forbidden": lines("what it must not do"),
            "kind": {"type": "string"},
            "base": {"type": "string", "description": "branch or commit (default HEAD)"}
        },
        "required": ["id", "goal", "scope", "acceptance", "verify", "timebox_min"],
        "additionalProperties": false
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    HostTool::new(
        WORKTREE_REQUEST_TOOL,
        "cedian's own host tool (trusted). Ask the cedian IDE for an isolated git worktree for a \
         parallel worker, with its brief (goal, scope.write, acceptance, verify, timebox_min); \
         never run `git worktree add` yourself. Returns the worktree id and path.",
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

    fn brief(id: &str) -> Value {
        json!({
            "id": id, "goal": "try it", "scope": {"write": ["src/**"]},
            "acceptance": ["tests pass"], "verify": ["cargo test"], "timebox_min": 30,
        })
    }

    #[test]
    fn request_creates_registers_and_stores_the_brief() {
        let repo = fixture();
        let out = request_worktree(&repo, &state(&repo), &args(brief("w1"))).unwrap();
        assert!(out.contains("branch cedian-worker/w1"), "{out}");
        assert!(repo.join(".worktrees/w1/a.txt").exists());
        let (reg, existed) = Registry::open(&state(&repo)).unwrap();
        assert!(existed);
        let head = reg.get("w1").unwrap();
        assert_eq!(head.task_title, "try it");
        assert_eq!(head.status, WorkerStatus::Running);
        let stored = std::fs::read_to_string(state(&repo).join("briefs/w1.1.json")).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&stored).unwrap(), brief("w1"));
    }

    #[test]
    fn a_request_without_a_whole_brief_names_every_missing_field_and_makes_no_tree() {
        let repo = fixture();
        let err = request_worktree(
            &repo,
            &state(&repo),
            &args(json!({"id": "w1", "goal": "try it", "scope": {"write": []}, "verify": [""]})),
        )
        .unwrap_err();
        assert!(
            err.contains("missing scope.write, acceptance, verify, timebox_min"),
            "{err}"
        );
        assert!(!repo.join(".worktrees/w1").exists());
        assert!(Registry::open(&state(&repo)).unwrap().0.get("w1").is_none());
        assert!(!state(&repo).join("briefs/w1.1.json").exists());
    }

    #[test]
    fn parallel_requests_each_keep_their_row() {
        let repo = fixture();
        let ids: Vec<String> = (0..8).map(|n| format!("p{n}")).collect();
        let results: Vec<Result<String, String>> = std::thread::scope(|s| {
            let handles: Vec<_> = ids
                .iter()
                .map(|id| {
                    let repo = &repo;
                    s.spawn(move || request_worktree(repo, &state(repo), &args(brief(id))))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for result in &results {
            assert!(result.is_ok(), "{result:?}");
        }
        let (reg, _) = Registry::open(&state(&repo)).unwrap();
        let rows: Vec<&str> = reg.all().map(|head| head.id.as_str()).collect();
        assert_eq!(rows, ids, "a parallel request lost its row");
    }

    #[test]
    fn a_brief_that_cannot_be_stored_leaves_no_tree_and_no_row() {
        let repo = fixture();
        let stuck = state(&repo).join("briefs/w1.1.json");
        std::fs::create_dir_all(&stuck).unwrap();
        let err = request_worktree(&repo, &state(&repo), &args(brief("w1"))).unwrap_err();
        assert!(err.contains("brief"), "{err}");
        assert!(
            !repo.join(".worktrees/w1").exists(),
            "no tree without its brief"
        );
        assert!(Registry::open(&state(&repo)).unwrap().0.get("w1").is_none());
        std::fs::remove_dir(&stuck).unwrap();
        request_worktree(&repo, &state(&repo), &args(brief("w1"))).expect("a retry succeeds");
        assert!(stuck.is_file());
    }

    #[test]
    fn duplicate_and_bad_ids_are_refused() {
        let repo = fixture();
        request_worktree(&repo, &state(&repo), &args(brief("w1"))).unwrap();
        let dup = request_worktree(&repo, &state(&repo), &args(brief("w1"))).unwrap_err();
        assert!(dup.contains("w1"), "{dup}");
        assert!(request_worktree(&repo, &state(&repo), &args(brief("../x"))).is_err());
        assert!(!repo.join(".worktrees/x").exists());
        let mut no_id = brief("");
        no_id.as_object_mut().unwrap().remove("id");
        let err = request_worktree(&repo, &state(&repo), &args(no_id)).unwrap_err();
        assert!(err.contains("missing id"), "{err}");
    }
}

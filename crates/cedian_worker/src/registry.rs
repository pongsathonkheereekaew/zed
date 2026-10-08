//! Worker registry: persisted heads at `workers.json` in the repo's cedian
//! state dir (ADR-0044), which the caller resolves and passes in.
//!
//! The registry is a MECHANISM record (§88): which workers exist, which
//! branch/worktree serves each task, and their lifecycle state. No
//! scheduling, no queue — OMP stays the only orchestrator.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Lifecycle state of one worker, serialized snake_case (`ready`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Ready,
    Running,
    Done,
    Stale,
    Failed,
}

/// One row in the registry: which branch/worktree serves a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerHead {
    pub id: String,
    pub branch: String,
    /// Repo-relative worktree path (`.worktrees/<id>`).
    pub worktree: String,
    pub task_title: String,
    pub kind: String,
    pub status: WorkerStatus,
    pub note: String,
}

/// Registry errors: duplicate/unknown ids, or registry file I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerError {
    Exists(String),
    NoSuch(String),
    Io(String),
    Git(String),
    Conflicted(Vec<String>),
    /// Worker id is not a safe path/branch segment.
    BadId(String),
    /// `workers.json` is corrupt or another snapshot version (fail closed).
    Snapshot(String),
    /// Branch has commits not in the main checkout — removing would lose them.
    NotMerged(String),
    /// The main checkout is not on the requested merge base.
    WrongBase {
        base: String,
        checked_out: String,
    },
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exists(id) => write!(f, "worker {id:?} already exists"),
            Self::NoSuch(id) => write!(f, "no worker {id:?}"),
            Self::Io(e) => write!(f, "worker registry io: {e}"),
            Self::Snapshot(e) => write!(f, "worker registry: {e}"),
            Self::Git(e) => write!(f, "worker git failed: {e}"),
            Self::Conflicted(files) => {
                write!(f, "worker merge refused (STALE): {}", files.join(", "))
            }
            Self::BadId(id) => write!(
                f,
                "bad worker id {id:?} (use 1-64 chars of [A-Za-z0-9._-], not starting with '.')"
            ),
            Self::NotMerged(branch) => write!(
                f,
                "branch {branch} has unmerged commits — merge-back first, or delete it with git yourself"
            ),
            Self::WrongBase { base, checked_out } => write!(
                f,
                "merge base {base:?} is not checked out (main checkout is on {checked_out:?})"
            ),
        }
    }
}

fn registry_path(state: &Path) -> PathBuf {
    state.join("workers.json")
}

/// `workers.json` schema version (ADR-0016 / P3). Bump on any shape change.
pub const REGISTRY_SNAPSHOT_VERSION: u32 = 1;

/// Persisted worker heads, shaped `{"snapshot_version": N, "workers": {id: head}}`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Registry {
    /// Missing in pre-P3 files → 0 → rejected.
    #[serde(default)]
    snapshot_version: u32,
    workers: BTreeMap<String, WorkerHead>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            snapshot_version: REGISTRY_SNAPSHOT_VERSION,
            workers: BTreeMap::new(),
        }
    }
}

impl Registry {
    /// Take the registry's lock under `state` (created if missing), held
    /// until the file is dropped. Every open-change-save holds it, the app's
    /// host tool and the CLI alike, so no writer saves over another's row.
    pub fn lock(state: &Path) -> Result<std::fs::File, WorkerError> {
        std::fs::create_dir_all(state).map_err(|e| WorkerError::Io(e.to_string()))?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(state.join("workers.lock"))
            .map_err(|e| WorkerError::Io(e.to_string()))?;
        file.lock().map_err(|e| WorkerError::Io(e.to_string()))?;
        Ok(file)
    }

    /// Open the registry; a missing file yields an empty one. Returns the
    /// registry plus whether the file already existed. An unreadable, corrupt
    /// or other-version file fails closed — never silently emptied, since the
    /// next save would forget live worktrees.
    pub fn open(state: &Path) -> Result<(Self, bool), WorkerError> {
        let path = registry_path(state);
        if !path.exists() {
            return Ok((Self::default(), false));
        }
        let raw = std::fs::read_to_string(&path).map_err(|e| WorkerError::Io(e.to_string()))?;
        let reg: Self = serde_json::from_str(&raw)
            .map_err(|e| WorkerError::Snapshot(format!("corrupt {}: {e}", path.display())))?;
        if reg.snapshot_version != REGISTRY_SNAPSHOT_VERSION {
            return Err(WorkerError::Snapshot(format!(
                "workers.json state too old (got v{}, want v{REGISTRY_SNAPSHOT_VERSION}), \
                 re-baseline: inspect `git worktree list`, then remove {}",
                reg.snapshot_version,
                path.display()
            )));
        }
        Ok((reg, true))
    }

    /// Persist into `state` (created if missing).
    pub fn save(&self, state: &Path) -> Result<(), WorkerError> {
        std::fs::create_dir_all(state).map_err(|e| WorkerError::Io(e.to_string()))?;
        let raw = serde_json::to_string_pretty(self).map_err(|e| WorkerError::Io(e.to_string()))?;
        std::fs::write(registry_path(state), raw).map_err(|e| WorkerError::Io(e.to_string()))?;
        Ok(())
    }

    /// Insert a head; duplicate ids are rejected (record, not a queue).
    pub fn insert(&mut self, head: WorkerHead) -> Result<(), WorkerError> {
        if self.workers.contains_key(&head.id) {
            return Err(WorkerError::Exists(head.id));
        }
        self.workers.insert(head.id.clone(), head);
        Ok(())
    }

    /// Look up a head by id.
    pub fn get(&self, id: &str) -> Option<&WorkerHead> {
        self.workers.get(id)
    }

    /// All heads by id order — the CLI `list` view.
    pub fn all(&self) -> impl Iterator<Item = &WorkerHead> {
        self.workers.values()
    }

    /// Mark status + note; unknown ids fail (no implicit creation).
    pub fn set_status(
        &mut self,
        id: &str,
        status: WorkerStatus,
        note: String,
    ) -> Result<(), WorkerError> {
        let head = self
            .workers
            .get_mut(id)
            .ok_or_else(|| WorkerError::NoSuch(id.to_string()))?;
        head.status = status;
        head.note = note;
        Ok(())
    }

    /// Drop a head, returning it when present.
    pub fn remove(&mut self, id: &str) -> Option<WorkerHead> {
        self.workers.remove(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_roundtrip() {
        let repo = std::env::temp_dir().join(format!("cedian-reg-test-{}", std::process::id()));
        std::fs::create_dir_all(&repo).unwrap();
        let (mut reg, existed) = Registry::open(&repo).unwrap();
        assert!(!existed);
        reg.insert(WorkerHead {
            id: "w1".into(),
            branch: "cedian-worker/w1".into(),
            worktree: ".worktrees/w1".into(),
            task_title: "fix login".into(),
            kind: "bug_fix".into(),
            status: WorkerStatus::Ready,
            note: String::new(),
        })
        .unwrap();
        assert!(
            reg.insert(WorkerHead {
                id: "w1".into(),
                branch: "b".into(),
                worktree: "w".into(),
                task_title: "t".into(),
                kind: "k".into(),
                status: WorkerStatus::Ready,
                note: String::new(),
            })
            .is_err()
        );
        reg.save(&repo).unwrap();
        let (reg2, existed2) = Registry::open(&repo).unwrap();
        assert!(existed2);
        assert_eq!(reg2.get("w1").unwrap().task_title, "fix login");
        std::fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn stale_or_corrupt_registry_fails_closed() {
        let repo = std::env::temp_dir().join(format!("cedian-reg-stale-{}", std::process::id()));
        std::fs::create_dir_all(&repo).unwrap();
        // Pre-P3 shape: no snapshot_version.
        std::fs::write(registry_path(&repo), r#"{"workers":{}}"#).unwrap();
        assert!(matches!(
            Registry::open(&repo),
            Err(WorkerError::Snapshot(_))
        ));
        std::fs::write(
            registry_path(&repo),
            r#"{"snapshot_version":99,"workers":{}}"#,
        )
        .unwrap();
        assert!(matches!(
            Registry::open(&repo),
            Err(WorkerError::Snapshot(_))
        ));
        std::fs::write(registry_path(&repo), "{not json").unwrap();
        assert!(matches!(
            Registry::open(&repo),
            Err(WorkerError::Snapshot(_))
        ));
        let _ = std::fs::remove_dir_all(&repo);
    }
}

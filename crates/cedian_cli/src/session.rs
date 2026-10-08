//! Review store for the CLI harness (headless stopgap, plan §§16–17, §75).
//!
//! One file per workdir, `review.json` in its state dir (ADR-0044), holding the current review
//! TASK: baseline texts (taken the first time a file is seen in the task —
//! task baseline, not per-turn), the turn log and the models that answered.
//! Versioned (`snapshot_version`): a mismatch or a corrupt file fails closed
//! with "re-baseline" — never silently misread (§75 herdr lesson 3).
//! `cedian review reset` deletes it to start a new task.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Bump on any schema change — older files fail closed.
pub const REVIEW_SNAPSHOT_VERSION: u32 = 3;

/// The CLI's single review task id (the app shell owns real task ids).
pub const CLI_TASK: &str = "cli";

/// Persisted review task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewStore {
    pub snapshot_version: u32,
    pub task_id: String,
    /// Buffer key → text at task start.
    pub baseline: BTreeMap<String, String>,
    /// Every turn that changed files, oldest first.
    pub turns: Vec<TurnRecord>,
    /// `provider/model` of every model that answered in a turn of this task
    /// (ADR-0039: a review is independent only of all of them).
    #[serde(default)]
    pub models: std::collections::BTreeSet<String>,
}

/// What produced a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TurnKind {
    Prompt,
    /// Inline edit (`edit <path> <range> <instruction>`).
    Edit,
}

/// One file as a turn left it. Pre/post texts come from disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnFile {
    /// Buffer key (`/rel`).
    pub file: String,
    pub before: String,
    pub after: String,
    /// Did not exist before the turn.
    pub created: bool,
}

/// One turn's file changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRecord {
    /// 1-based, in the order turns were recorded.
    pub n: u32,
    #[serde(flatten)]
    pub kind: TurnKind,
    /// First line of the prompt (or the edit description).
    pub label: String,
    pub files: Vec<TurnFile>,
}

impl ReviewStore {
    /// Empty task.
    pub fn new() -> Self {
        Self {
            snapshot_version: REVIEW_SNAPSHOT_VERSION,
            task_id: CLI_TASK.to_string(),
            baseline: BTreeMap::new(),
            turns: Vec::new(),
            models: Default::default(),
        }
    }

    /// The newest turn that changed `file` (a buffer key).
    pub fn last_turn_touching(&self, file: &str) -> Option<u32> {
        self.turns
            .iter()
            .rev()
            .find(|t| t.files.iter().any(|f| f.file == file))
            .map(|t| t.n)
    }

    /// Append a turn (numbered here); no-op when it changed nothing.
    pub fn record_turn(
        &mut self,
        kind: TurnKind,
        label: &str,
        files: Vec<TurnFile>,
    ) -> Option<u32> {
        if files.is_empty() {
            return None;
        }
        let n = self.turns.last().map_or(1, |t| t.n + 1);
        let label: String = label
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect();
        self.turns.push(TurnRecord {
            n,
            kind,
            label,
            files,
        });
        Some(n)
    }

    /// Record the task-start text for a file the first time it is seen.
    pub fn baseline_once(&mut self, key: &Path, text: &str) {
        self.baseline
            .entry(key.to_string_lossy().into_owned())
            .or_insert_with(|| text.to_string());
    }
}

impl Default for ReviewStore {
    fn default() -> Self {
        Self::new()
    }
}

fn store_path(workdir: &Path) -> Result<PathBuf, String> {
    crate::state::file(workdir, "review.json")
}

/// Load the review task. `Ok(None)` when no task exists yet. Corrupt or
/// version-mismatched files fail closed.
pub fn load(workdir: &Path) -> Result<Option<ReviewStore>, String> {
    let path = store_path(workdir)?;
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let store: ReviewStore = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "corrupt {}: {e} — re-baseline: run `cedian review reset`",
            path.display()
        )
    })?;
    if store.snapshot_version != REVIEW_SNAPSHOT_VERSION {
        return Err(format!(
            "review state too old (got v{}, want v{REVIEW_SNAPSHOT_VERSION}), re-baseline: run `cedian review reset`",
            store.snapshot_version
        ));
    }
    Ok(Some(store))
}

/// Save atomically (write temp + rename) so a crash never leaves half a file.
pub fn save(workdir: &Path, store: &ReviewStore) -> Result<(), String> {
    let path = store_path(workdir)?;
    let raw = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, raw).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// Delete the review task.
pub fn reset(workdir: &Path) {
    if let Ok(path) = store_path(workdir) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> crate::test_dir::TestDir {
        crate::test_dir::TestDir::new(&format!("review-store-{tag}"))
    }

    #[test]
    fn roundtrip_and_baseline_once() {
        let dir = tmp("rt");
        assert!(load(&dir).unwrap().is_none());
        let mut store = ReviewStore::new();
        store.baseline_once(Path::new("/a.rs"), "v1");
        store.baseline_once(Path::new("/a.rs"), "v2");
        save(&dir, &store).unwrap();
        let back = load(&dir).unwrap().unwrap();
        assert_eq!(back.baseline["/a.rs"], "v1", "task baseline kept");
        let file = TurnFile {
            file: "/a.rs".into(),
            before: "v1".into(),
            after: "v3".into(),
            created: false,
        };
        let mut back = back;
        assert_eq!(
            back.record_turn(TurnKind::Prompt, "fix\nmore", vec![]),
            None
        );
        assert_eq!(
            back.record_turn(TurnKind::Prompt, "fix\nmore", vec![file.clone()]),
            Some(1)
        );
        assert_eq!(
            back.record_turn(TurnKind::Edit, "edit", vec![file]),
            Some(2)
        );
        save(&dir, &back).unwrap();
        let back = load(&dir).unwrap().unwrap();
        assert_eq!(back.turns[0].label, "fix");
        assert_eq!(back.turns[1].kind, TurnKind::Edit);
        reset(&dir);
        assert!(load(&dir).unwrap().is_none());
    }

    #[test]
    fn version_mismatch_fails_closed() {
        let dir = tmp("ver");
        let mut store = ReviewStore::new();
        store.snapshot_version = 0;
        save(&dir, &store).unwrap();
        assert!(load(&dir).unwrap_err().contains("re-baseline"));
        reset(&dir);
        assert!(load(&dir).unwrap().is_none());
    }
}

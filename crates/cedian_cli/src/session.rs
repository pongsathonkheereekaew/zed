//! Review store for the CLI harness (headless stopgap, plan §§16–17, §75).
//!
//! One file per workdir, `.cedian/review.json`, holding the current review
//! TASK: baseline texts (taken the first time a file is seen in the task —
//! task baseline, not per-turn), the cedian-owned `AgentEdit` records (§17 R1:
//! never derived from the OMP transcript), and the user's hunk resolutions.
//! Versioned (`snapshot_version`): a mismatch or a corrupt file fails closed
//! with "re-baseline" — never silently misread (§75 herdr lesson 3).
//! `cedian review reset` deletes it to start a new task.

use cedian_review::{AgentEdit, Baseline, ProvenanceStore, StatusRecord};
use cedian_workspace::Version;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Bump on any schema change — older files fail closed.
pub const REVIEW_SNAPSHOT_VERSION: u32 = 2;

/// The CLI's single review task id (the app shell owns real task ids).
pub const CLI_TASK: &str = "cli";

/// Persisted review task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewStore {
    pub snapshot_version: u32,
    pub task_id: String,
    /// Buffer key → text at task start.
    pub baseline: BTreeMap<String, String>,
    /// §17 `AgentEdit` records, oldest first.
    pub provenance: Vec<AgentEdit>,
    /// User resolutions keyed by hunk identity.
    pub statuses: Vec<StatusRecord>,
    /// Every turn that changed files, oldest first (P6: revert turn).
    pub turns: Vec<TurnRecord>,
}

/// What produced a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TurnKind {
    Prompt,
    /// Inline edit (`edit <path> <range> <instruction>`).
    Edit,
    /// `revert-turn <of>` — revert it again to redo `of`.
    Revert {
        of: u32,
    },
}

/// One file as a turn left it. Pre/post texts come from disk, so files
/// with no `AgentEdit` (UNATTRIBUTED) are covered too.
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
    /// First line of the prompt (or the edit / revert description).
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
            provenance: Vec::new(),
            statuses: Vec::new(),
            turns: Vec::new(),
        }
    }

    /// Append a turn (numbered here); no-op when it changed nothing.
    /// The newest turn that changed `file` (a buffer key).
    pub fn last_turn_touching(&self, file: &str) -> Option<u32> {
        self.turns
            .iter()
            .rev()
            .find(|t| t.files.iter().any(|f| f.file == file))
            .map(|t| t.n)
    }

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

    /// Append one agent edit record (keyed task + tool call).
    pub fn record(&mut self, edit: AgentEdit) {
        let mut store = ProvenanceStore::from_records(std::mem::take(&mut self.provenance));
        store.record(edit);
        self.provenance = store.all();
    }

    /// Baseline in tracker shape (versions re-anchor at 0: review diffs text).
    pub fn tracker_inputs(&self) -> (Baseline, HashMap<PathBuf, String>) {
        let mut baseline = Baseline::new();
        let mut texts = HashMap::new();
        for (key, text) in &self.baseline {
            let key = PathBuf::from(key);
            baseline.snapshot(&key, Version(0));
            texts.insert(key, text.clone());
        }
        (baseline, texts)
    }
}

impl Default for ReviewStore {
    fn default() -> Self {
        Self::new()
    }
}

fn store_path(workdir: &Path) -> PathBuf {
    workdir.join(".cedian").join("review.json")
}

/// Load the review task. `Ok(None)` when no task exists yet. Corrupt or
/// version-mismatched files fail closed.
pub fn load(workdir: &Path) -> Result<Option<ReviewStore>, String> {
    let path = store_path(workdir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => {
            if workdir.join(".cedian").join("baseline.json").exists() {
                return Err(
                    "review state too old (.cedian/baseline.json), re-baseline: run `cedian review reset`"
                        .to_string(),
                );
            }
            return Ok(None);
        }
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
    let path = store_path(workdir);
    std::fs::create_dir_all(path.parent().unwrap_or(workdir)).map_err(|e| e.to_string())?;
    let raw = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, raw).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// Delete the review task (and the legacy baseline file).
pub fn reset(workdir: &Path) {
    let _ = std::fs::remove_file(store_path(workdir));
    let _ = std::fs::remove_file(workdir.join(".cedian").join("baseline.json"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cedian-review-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn roundtrip_and_baseline_once() {
        let dir = tmp("rt");
        assert!(load(&dir).unwrap().is_none());
        let mut store = ReviewStore::new();
        store.baseline_once(Path::new("/a.rs"), "v1");
        store.baseline_once(Path::new("/a.rs"), "v2");
        store.record(AgentEdit {
            tool_call_id: "c1".into(),
            task_id: CLI_TASK.into(),
            file: "/a.rs".into(),
            before: "v1".into(),
            after: "v3".into(),
            timestamp_ms: 1,
        });
        save(&dir, &store).unwrap();
        let back = load(&dir).unwrap().unwrap();
        assert_eq!(back.baseline["/a.rs"], "v1", "task baseline kept");
        assert_eq!(back.provenance.len(), 1);
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
            back.record_turn(TurnKind::Revert { of: 1 }, "revert", vec![file]),
            Some(2)
        );
        save(&dir, &back).unwrap();
        let back = load(&dir).unwrap().unwrap();
        assert_eq!(back.turns[0].label, "fix");
        assert_eq!(back.turns[1].kind, TurnKind::Revert { of: 1 });
        reset(&dir);
        assert!(load(&dir).unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn version_mismatch_and_legacy_fail_closed() {
        let dir = tmp("ver");
        let mut store = ReviewStore::new();
        store.snapshot_version = 0;
        save(&dir, &store).unwrap();
        assert!(load(&dir).unwrap_err().contains("re-baseline"));
        reset(&dir);
        std::fs::write(dir.join(".cedian").join("baseline.json"), "{}").unwrap();
        assert!(load(&dir).unwrap_err().contains("re-baseline"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

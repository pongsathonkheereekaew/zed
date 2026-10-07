//! Review tracker: baseline → current diffs + hunk accept/reject (plan §18).
//!
//! Semantics: accept = mark (code already in the buffer); reject = inverse
//! patch to baseline state. STALE (user edited after agent) never auto-rejects
//! — compare/restore manually. Bulk accept-all SKIPS `Unattributed` hunks (§17
//! R2: per-hunk resolve only).
//!
//! Attribution (§16/§17): a hunk belongs to the task only when the latest
//! `AgentEdit` for its file explains it. Files with no agent record →
//! `Unattributed`; hunks overlapping changes made AFTER the agent's last
//! write (agent text → current) → `Stale`. User resolutions are keyed by hunk
//! identity (before range + after text), not index, so they survive rebuilds
//! and persist across sessions via [`StatusRecord`].
//!
//! Debounce (§20): diff rebuilds queue on edit-complete and flush at 50–100ms;
//! streaming tokens and tool progress never trigger recomputation. Headless:
//! the owner calls `request_rebuild` + `rebuild_due`; GPUI ticks it per frame.

use crate::{AgentEdit, Baseline, FileDiff, Hunk, HunkStatus, line_diff};
use cedian_workspace::{TextEdit, Version, WorkspaceHost};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Rebuild debounce window (§20: 50–100ms).
pub const REBUILD_DEBOUNCE: Duration = Duration::from_millis(50);

/// Tracker failures (caller-visible).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackerError {
    /// Host edit failed during inverse patch.
    Host(String),
    /// No diff computed for this path yet.
    NoDiff { path: PathBuf },
    /// Hunk index out of range.
    BadHunk { path: PathBuf, index: usize },
    /// Transition not allowed (e.g. resolving `Interrupted` directly).
    BadTransition { status: HunkStatus },
    /// Buffer changed since the diff was built — rebuild and look again.
    Outdated { path: PathBuf },
}

impl std::fmt::Display for TrackerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDiff { path } => write!(f, "no review diff for {}", path.display()),
            Self::BadHunk { path, index } => write!(f, "no hunk {index} in {}", path.display()),
            Self::BadTransition {
                status: HunkStatus::Stale,
            } => write!(
                f,
                "hunk is STALE (changed after the agent edit) — compare and restore manually"
            ),
            Self::BadTransition { status } => {
                write!(f, "hunk transition not allowed from {status:?}")
            }
            Self::Host(e) => write!(f, "host edit failed: {e}"),
            Self::Outdated { path } => write!(
                f,
                "{} changed since the review diff was built — re-run review",
                path.display()
            ),
        }
    }
}

impl std::error::Error for TrackerError {}

/// Stable hunk identity: survives rebuilds that shift indices.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HunkKey {
    pub before_start: usize,
    pub before_count: usize,
    /// Exact after-side lines (joined with `\n`).
    pub after_text: String,
}

impl HunkKey {
    fn of(after: &str, hunk: &Hunk) -> Self {
        Self {
            before_start: hunk.before_start,
            before_count: hunk.before_count,
            after_text: after
                .lines()
                .skip(hunk.after_start)
                .take(hunk.after_count)
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

/// One persisted user resolution / manual status (headless store record).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusRecord {
    pub path: PathBuf,
    pub key: HunkKey,
    pub status: HunkStatus,
}

/// Per-task review state: baseline + latest agent texts + current diffs.
pub struct ReviewTracker {
    task_id: String,
    baseline: Baseline,
    baseline_texts: HashMap<PathBuf, String>,
    /// Latest agent-produced text per file (from the §17 `AgentEdit` store).
    agent_texts: HashMap<PathBuf, String>,
    /// User resolutions + manual transitions, keyed by hunk identity.
    resolved: HashMap<(PathBuf, HunkKey), HunkStatus>,
    diffs: HashMap<PathBuf, FileDiff>,
    rebuild_queued: bool,
    last_rebuild: Option<Instant>,
}

impl ReviewTracker {
    /// Tracker for one task, seeded with baseline versions + texts.
    pub fn new(
        task_id: &str,
        baseline: Baseline,
        baseline_texts: HashMap<PathBuf, String>,
    ) -> Self {
        Self {
            task_id: task_id.to_string(),
            baseline,
            baseline_texts,
            agent_texts: HashMap::new(),
            resolved: HashMap::new(),
            diffs: HashMap::new(),
            rebuild_queued: false,
            last_rebuild: None,
        }
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    /// Feed this task's `AgentEdit` records: the newest record per file (by
    /// timestamp, then input order) defines what the agent last wrote.
    /// Records of other tasks are ignored.
    pub fn attribute(&mut self, edits: &[AgentEdit]) {
        let mut newest: HashMap<PathBuf, (u64, usize, &AgentEdit)> = HashMap::new();
        for (i, e) in edits.iter().enumerate() {
            if e.task_id != self.task_id || e.tool_call_id.is_empty() {
                continue;
            }
            let path = PathBuf::from(&e.file);
            let replace = newest
                .get(&path)
                .map(|(ts, idx, _)| (e.timestamp_ms, i) >= (*ts, *idx))
                .unwrap_or(true);
            if replace {
                newest.insert(path, (e.timestamp_ms, i, e));
            }
        }
        for (path, (_, _, e)) in newest {
            self.agent_texts.insert(path, e.after.clone());
        }
        self.request_rebuild();
    }

    /// Restore persisted resolutions (applied on the next rebuild).
    pub fn restore_statuses(&mut self, records: Vec<StatusRecord>) {
        for r in records {
            self.resolved.insert((r.path, r.key), r.status);
        }
        self.request_rebuild();
    }

    /// Resolutions to persist (sorted for stable files).
    pub fn status_records(&self) -> Vec<StatusRecord> {
        let mut out: Vec<StatusRecord> = self
            .resolved
            .iter()
            .map(|((path, key), status)| StatusRecord {
                path: path.clone(),
                key: key.clone(),
                status: *status,
            })
            .collect();
        out.sort_by(|a, b| {
            (&a.path, a.key.before_start, &a.key.after_text).cmp(&(
                &b.path,
                b.key.before_start,
                &b.key.after_text,
            ))
        });
        out
    }

    /// Queue a rebuild (edit completed). Cheap; never diffs inline.
    pub fn request_rebuild(&mut self) {
        self.rebuild_queued = true;
    }

    /// Rebuild if queued AND the debounce window elapsed. Returns rebuilt paths.
    pub fn rebuild_due(&mut self, host: &dyn WorkspaceHost) -> Vec<PathBuf> {
        if !self.rebuild_queued {
            return Vec::new();
        }
        if let Some(last) = self.last_rebuild {
            if last.elapsed() < REBUILD_DEBOUNCE {
                return Vec::new();
            }
        }
        self.rebuild_now(host)
    }

    /// Rebuild immediately, ignoring the debounce (one-shot CLI, tests).
    pub fn rebuild_now(&mut self, host: &dyn WorkspaceHost) -> Vec<PathBuf> {
        self.rebuild_queued = false;
        self.last_rebuild = Some(Instant::now());
        let mut rebuilt = Vec::new();
        for path in self.baseline.paths() {
            let before = self.baseline_texts.get(&path).cloned().unwrap_or_default();
            let after = host.read_buffer(&path).unwrap_or_default();
            let hunks = line_diff(&before, &after);
            let post_agent = self.agent_texts.get(&path).map(|a| line_diff(a, &after));
            let statuses = hunks
                .iter()
                .map(|h| {
                    let key = HunkKey::of(&after, h);
                    if let Some(s) = self.resolved.get(&(path.clone(), key)) {
                        return *s;
                    }
                    derive_status(h, post_agent.as_deref())
                })
                .collect();
            self.diffs.insert(
                path.clone(),
                FileDiff {
                    path: path.to_string_lossy().into_owned(),
                    hunks,
                    statuses,
                    snapshot: after,
                },
            );
            rebuilt.push(path);
        }
        rebuilt
    }

    /// Diff for one path (after a rebuild).
    pub fn diff(&self, path: &Path) -> Result<&FileDiff, TrackerError> {
        self.diffs.get(path).ok_or_else(|| TrackerError::NoDiff {
            path: path.to_path_buf(),
        })
    }

    /// Paths with a computed diff (sorted, stable for rendering).
    pub fn paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self.diffs.keys().cloned().collect();
        paths.sort();
        paths
    }

    /// Accept one hunk: mark only (code already in the buffer). `Interrupted`
    /// must reconcile to `Unattributed` first; terminal states are idempotent.
    pub fn accept_hunk(&mut self, path: &Path, index: usize) -> Result<(), TrackerError> {
        let (status, key) = self.hunk(path, index)?;
        match status {
            HunkStatus::Pending | HunkStatus::Unattributed | HunkStatus::Stale => {
                self.set_resolved(path, index, key, HunkStatus::Accepted);
                Ok(())
            }
            HunkStatus::Interrupted => Err(TrackerError::BadTransition { status }),
            HunkStatus::Accepted | HunkStatus::Rejected => Ok(()),
        }
    }

    /// Reject one hunk: inverse patch back to baseline lines, as one host
    /// transaction. Refuses `Interrupted` and `Stale` (§18: never auto-reject
    /// a hunk the user changed after the agent), and refuses when the buffer
    /// moved since the diff was built (line positions would be wrong).
    pub fn reject_hunk(
        &mut self,
        path: &Path,
        index: usize,
        host: &dyn WorkspaceHost,
    ) -> Result<Version, TrackerError> {
        let (status, key) = self.hunk(path, index)?;
        match status {
            HunkStatus::Interrupted | HunkStatus::Stale => {
                return Err(TrackerError::BadTransition { status });
            }
            HunkStatus::Accepted | HunkStatus::Rejected => {
                return host
                    .buffer_version(path)
                    .ok_or_else(|| TrackerError::NoDiff {
                        path: path.to_path_buf(),
                    });
            }
            HunkStatus::Pending | HunkStatus::Unattributed => {}
        }
        let diff = self.diff(path)?;
        let hunk = diff.hunks[index].clone();
        let after = host.read_buffer(path).unwrap_or_default();
        if after != diff.snapshot {
            return Err(TrackerError::Outdated {
                path: path.to_path_buf(),
            });
        }
        let before = self.baseline_texts.get(path).cloned().unwrap_or_default();
        let before_lines: Vec<&str> = before.lines().collect();
        let want: Vec<&str> = before_lines
            .iter()
            .skip(hunk.before_start)
            .take(hunk.before_count)
            .copied()
            .collect();
        let (start_off, end_off) = line_range_offsets(&after, hunk.after_start, hunk.after_count);
        let current = host
            .buffer_version(path)
            .ok_or_else(|| TrackerError::NoDiff {
                path: path.to_path_buf(),
            })?;
        // Restore the baseline's own line ending at EOF (no newline invented).
        let at_eof = hunk.before_start + hunk.before_count == before_lines.len();
        let replacement = if want.is_empty() {
            String::new()
        } else if at_eof && !before.ends_with('\n') {
            want.join("\n")
        } else {
            let mut s = want.join("\n");
            s.push('\n');
            s
        };
        let result = host
            .apply_edit(
                path,
                current,
                &TextEdit {
                    start: start_off,
                    end: end_off,
                    replacement,
                },
            )
            .map_err(|e| TrackerError::Host(e.to_string()))?;
        self.set_resolved(path, index, key, HunkStatus::Rejected);
        self.request_rebuild();
        Ok(result.new_version)
    }

    /// Accept every `Pending` and `Stale` hunk. SKIPS `Unattributed` and
    /// `Interrupted` (§17 R2). Returns accepted `(path, index)` pairs.
    pub fn accept_all(&mut self) -> Vec<(PathBuf, usize)> {
        let mut targets = Vec::new();
        for path in self.paths() {
            let diff = &self.diffs[&path];
            for (i, status) in diff.statuses.iter().enumerate() {
                if matches!(status, HunkStatus::Pending | HunkStatus::Stale) {
                    targets.push((path.clone(), i, HunkKey::of(&diff.snapshot, &diff.hunks[i])));
                }
            }
        }
        targets
            .into_iter()
            .map(|(path, i, key)| {
                self.set_resolved(&path, i, key, HunkStatus::Accepted);
                (path, i)
            })
            .collect()
    }

    /// Drive an allowed non-user transition (§18 R3): `Interrupted →
    /// Unattributed`, `Pending|Unattributed → Stale`. Same-state is a no-op.
    pub fn set_status(
        &mut self,
        path: &Path,
        index: usize,
        status: HunkStatus,
    ) -> Result<(), TrackerError> {
        let (current, key) = self.hunk(path, index)?;
        let allowed = match (current, status) {
            (HunkStatus::Interrupted, HunkStatus::Unattributed) => true,
            (HunkStatus::Pending, HunkStatus::Stale) => true,
            (HunkStatus::Unattributed, HunkStatus::Stale) => true,
            (a, b) if a == b => true,
            _ => false,
        };
        if !allowed {
            return Err(TrackerError::BadTransition { status: current });
        }
        self.set_resolved(path, index, key, status);
        Ok(())
    }

    fn hunk(&self, path: &Path, index: usize) -> Result<(HunkStatus, HunkKey), TrackerError> {
        let diff = self.diff(path)?;
        let hunk = diff.hunks.get(index).ok_or_else(|| TrackerError::BadHunk {
            path: path.to_path_buf(),
            index,
        })?;
        Ok((diff.statuses[index], HunkKey::of(&diff.snapshot, hunk)))
    }

    fn set_resolved(&mut self, path: &Path, index: usize, key: HunkKey, status: HunkStatus) {
        if let Some(diff) = self.diffs.get_mut(path) {
            diff.statuses[index] = status;
        }
        self.resolved.insert((path.to_path_buf(), key), status);
    }
}

/// Status for a hunk with no recorded resolution. No agent record for the
/// file → `Unattributed`; overlaps a change made after the agent's last write
/// → `Stale`; else `Pending` (attributed, awaiting review).
fn derive_status(hunk: &Hunk, post_agent: Option<&[Hunk]>) -> HunkStatus {
    let Some(post) = post_agent else {
        return HunkStatus::Unattributed;
    };
    let (hs, he) = (hunk.after_start, hunk.after_start + hunk.after_count);
    let touched = post.iter().any(|p| {
        let (ps, pe) = (p.after_start, p.after_start + p.after_count);
        if hs == he || ps == pe {
            ps <= he && hs <= pe
        } else {
            ps < he && hs < pe
        }
    });
    if touched {
        HunkStatus::Stale
    } else {
        HunkStatus::Pending
    }
}

/// Byte offsets of `count` lines starting at line `start` (newlines included).
fn line_range_offsets(text: &str, start: usize, count: usize) -> (usize, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let mut off = 0;
    for line in lines.iter().take(start) {
        off += line.len() + 1;
    }
    let start_off = off.min(text.len());
    for line in lines.iter().skip(start).take(count) {
        off += line.len() + 1;
    }
    (start_off, off.min(text.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Record what the agent just wrote (§17 snapshot at tool end).
    fn agent_wrote(tracker: &mut ReviewTracker, store: &cedian_workspace::HostTools) {
        tracker.attribute(&[AgentEdit {
            tool_call_id: "c1".to_string(),
            task_id: "task-1".to_string(),
            file: "/a.rs".to_string(),
            before: String::new(),
            after: store.read_buffer(Path::new("/a.rs")).unwrap(),
            timestamp_ms: 1,
        }]);
    }

    fn edit(store: &cedian_workspace::HostTools, start: usize, end: usize, text: &str) {
        let v = store.buffer_version(Path::new("/a.rs")).unwrap();
        store
            .apply_edit(
                Path::new("/a.rs"),
                v,
                &TextEdit {
                    start,
                    end,
                    replacement: text.into(),
                },
            )
            .unwrap();
    }

    fn statuses(tracker: &ReviewTracker) -> Vec<HunkStatus> {
        tracker.diff(Path::new("/a.rs")).unwrap().statuses.clone()
    }

    #[test]
    fn no_agent_record_is_unattributed() {
        let (store, mut tracker) = setup();
        edit(&store, 4, 7, "TWO"); // e.g. a user edit: nothing attributes it
        tracker.rebuild_now(&store);
        assert_eq!(statuses(&tracker), vec![HunkStatus::Unattributed]);
        assert!(tracker.accept_all().is_empty(), "bulk accept skips it");
    }

    #[test]
    fn user_edit_after_agent_goes_stale_and_reject_refuses() {
        let (store, mut tracker) = setup();
        edit(&store, 4, 7, "TWO"); // agent: two → TWO
        agent_wrote(&mut tracker, &store);
        edit(&store, 4, 7, "Two"); // user rewrites the same line afterwards
        tracker.rebuild_now(&store);
        assert_eq!(statuses(&tracker), vec![HunkStatus::Stale]);
        assert_eq!(
            tracker.reject_hunk(Path::new("/a.rs"), 0, &store),
            Err(TrackerError::BadTransition {
                status: HunkStatus::Stale
            })
        );
        assert_eq!(
            store.read_buffer(Path::new("/a.rs")).unwrap(),
            "one\nTwo\nthree\n",
            "user text untouched"
        );
    }

    #[test]
    fn untouched_agent_hunk_stays_pending_beside_user_edit() {
        let (store, mut tracker) = setup();
        edit(&store, 0, 3, "ONE"); // agent: line 0
        agent_wrote(&mut tracker, &store);
        edit(&store, 8, 13, "THREE"); // user: line 2, separate hunk
        tracker.rebuild_now(&store);
        assert_eq!(
            statuses(&tracker),
            vec![HunkStatus::Pending, HunkStatus::Stale]
        );
    }

    #[test]
    fn accept_survives_rebuild_and_roundtrips_records() {
        let (store, mut tracker) = setup();
        edit(&store, 0, 3, "ONE");
        edit(&store, 8, 11, "TWO");
        agent_wrote(&mut tracker, &store);
        tracker.rebuild_now(&store);
        tracker.accept_hunk(Path::new("/a.rs"), 1).unwrap();
        // Reject hunk 0 → hunk count shrinks; the accepted one keeps its status.
        tracker.reject_hunk(Path::new("/a.rs"), 0, &store).unwrap();
        tracker.rebuild_now(&store);
        assert_eq!(statuses(&tracker), vec![HunkStatus::Accepted]);
        // Persist → fresh tracker (next CLI invocation) → same status.
        let records = tracker.status_records();
        let (_, mut fresh) = setup();
        agent_wrote(&mut fresh, &store);
        fresh.restore_statuses(records);
        fresh.rebuild_now(&store);
        assert_eq!(statuses(&fresh), vec![HunkStatus::Accepted]);
    }

    #[test]
    fn reject_refuses_outdated_diff() {
        let (store, mut tracker) = setup();
        edit(&store, 4, 7, "TWO");
        agent_wrote(&mut tracker, &store);
        tracker.rebuild_now(&store);
        edit(&store, 0, 0, "zero\n"); // shifts every line before reject
        assert!(matches!(
            tracker.reject_hunk(Path::new("/a.rs"), 0, &store),
            Err(TrackerError::Outdated { .. })
        ));
    }

    #[test]
    fn reject_keeps_missing_final_newline() {
        let store = cedian_workspace::HostTools::new(Path::new("/"));
        store.open(Path::new("/a.rs"), "one\ntwo");
        let mut baseline = Baseline::new();
        baseline.snapshot(Path::new("/a.rs"), Version(0));
        let texts = HashMap::from([(PathBuf::from("/a.rs"), "one\ntwo".to_string())]);
        let mut tracker = ReviewTracker::new("task-1", baseline, texts);
        edit(&store, 4, 7, "TWO");
        agent_wrote(&mut tracker, &store);
        tracker.rebuild_now(&store);
        tracker.reject_hunk(Path::new("/a.rs"), 0, &store).unwrap();
        assert_eq!(store.read_buffer(Path::new("/a.rs")).unwrap(), "one\ntwo");
    }

    fn setup() -> (cedian_workspace::HostTools, ReviewTracker) {
        let store = cedian_workspace::HostTools::new(Path::new("/"));
        store.open(Path::new("/a.rs"), "one\ntwo\nthree\n");
        let mut baseline = Baseline::new();
        baseline.snapshot(Path::new("/a.rs"), Version(0));
        let mut texts = HashMap::new();
        texts.insert(PathBuf::from("/a.rs"), "one\ntwo\nthree\n".to_string());
        (store, ReviewTracker::new("task-1", baseline, texts))
    }

    #[test]
    fn rebuild_finds_agent_hunk() {
        let (store, mut tracker) = setup();
        store
            .apply_edit(
                Path::new("/a.rs"),
                Version(0),
                &TextEdit {
                    start: 4,
                    end: 7,
                    replacement: "TWO".into(),
                },
            )
            .unwrap();
        tracker.request_rebuild();
        // Debounce: force by resetting the clock.
        agent_wrote(&mut tracker, &store);
        tracker.last_rebuild = None;
        tracker.request_rebuild();
        let rebuilt = tracker.rebuild_due(&store);
        assert_eq!(rebuilt, vec![PathBuf::from("/a.rs")]);
        assert_eq!(tracker.diff(Path::new("/a.rs")).unwrap().len(), 1);
    }

    #[test]
    fn accept_marks_reject_restores() {
        let (store, mut tracker) = setup();
        store
            .apply_edit(
                Path::new("/a.rs"),
                Version(0),
                &TextEdit {
                    start: 4,
                    end: 7,
                    replacement: "TWO".into(),
                },
            )
            .unwrap();
        agent_wrote(&mut tracker, &store);
        tracker.last_rebuild = None;
        tracker.request_rebuild();
        tracker.rebuild_due(&store);
        tracker.accept_hunk(Path::new("/a.rs"), 0).unwrap();
        assert_eq!(
            tracker.diff(Path::new("/a.rs")).unwrap().statuses[0],
            HunkStatus::Accepted
        );

        let (store2, mut tracker2) = setup();
        store2
            .apply_edit(
                Path::new("/a.rs"),
                Version(0),
                &TextEdit {
                    start: 4,
                    end: 7,
                    replacement: "TWO".into(),
                },
            )
            .unwrap();
        agent_wrote(&mut tracker2, &store2);
        tracker2.last_rebuild = None;
        tracker2.request_rebuild();
        tracker2.rebuild_due(&store2);
        tracker2
            .reject_hunk(Path::new("/a.rs"), 0, &store2)
            .unwrap();
        assert_eq!(
            store2.read_buffer(Path::new("/a.rs")).unwrap(),
            "one\ntwo\nthree\n"
        );
    }

    #[test]
    fn accept_all_covers_pending_and_stale() {
        let (store, mut tracker) = setup();
        store
            .apply_edit(
                Path::new("/a.rs"),
                Version(0),
                &TextEdit {
                    start: 0,
                    end: 3,
                    replacement: "ONE".into(),
                },
            )
            .unwrap();
        store
            .apply_edit(
                Path::new("/a.rs"),
                Version(1),
                &TextEdit {
                    start: 8,
                    end: 11,
                    replacement: "TWO".into(),
                },
            )
            .unwrap();
        agent_wrote(&mut tracker, &store);
        tracker.last_rebuild = None;
        tracker.request_rebuild();
        tracker.rebuild_due(&store);
        assert_eq!(tracker.diff(Path::new("/a.rs")).unwrap().len(), 2);
        tracker
            .set_status(Path::new("/a.rs"), 0, HunkStatus::Stale)
            .unwrap();
        let accepted = tracker.accept_all();
        assert_eq!(accepted.len(), 2, "stale + pending both bulk-accepted");
    }

    #[test]
    fn interrupted_reconciles_to_unattributed_then_resolves() {
        let (store, mut tracker) = setup();
        store
            .apply_edit(
                Path::new("/a.rs"),
                Version(0),
                &TextEdit {
                    start: 4,
                    end: 7,
                    replacement: "TWO".into(),
                },
            )
            .unwrap();
        agent_wrote(&mut tracker, &store);
        tracker.last_rebuild = None;
        tracker.request_rebuild();
        tracker.rebuild_due(&store);
        // Crash path: force Interrupted (test-only direct write), then drive
        // the allowed chain Interrupted → Unattributed → per-hunk accept.
        tracker.diffs.get_mut(Path::new("/a.rs")).unwrap().statuses[0] = HunkStatus::Interrupted;
        tracker
            .set_status(Path::new("/a.rs"), 0, HunkStatus::Unattributed)
            .unwrap();
        assert!(
            tracker.accept_all().is_empty(),
            "bulk accept skips unattributed"
        );
        tracker.accept_hunk(Path::new("/a.rs"), 0).unwrap();
        assert_eq!(
            tracker.diff(Path::new("/a.rs")).unwrap().statuses[0],
            HunkStatus::Accepted
        );
    }

    #[test]
    fn bad_transitions_fail() {
        let (store, mut tracker) = setup();
        store
            .apply_edit(
                Path::new("/a.rs"),
                Version(0),
                &TextEdit {
                    start: 4,
                    end: 7,
                    replacement: "TWO".into(),
                },
            )
            .unwrap();
        agent_wrote(&mut tracker, &store);
        tracker.last_rebuild = None;
        tracker.request_rebuild();
        tracker.rebuild_due(&store);
        // Pending → Unattributed directly is NOT allowed (only via Interrupted).
        assert!(
            tracker
                .set_status(Path::new("/a.rs"), 0, HunkStatus::Unattributed)
                .is_err()
        );
    }
}

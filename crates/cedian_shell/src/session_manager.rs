//! Session manager: task lifecycle over OMP sessions (plan §2.5).
//!
//! Thin model over new/switch/archive + resume-checkbox state (§76) + storage
//! meter per task. OMP owns transcripts; this owns the mapping list. No I/O:
//! the owner persists entries (CLI: `.cedian/sessions.json`; app: real store).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One managed session entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub task_id: String,
    pub title: String,
    pub workspace: PathBuf,
    pub session_dir: PathBuf,
    /// Resume grant (§76): checkbox state; restart consumes it (see
    /// `SessionBinding::on_restart` — the manager mirrors that, not replaces).
    pub resume_granted: bool,
    /// Bytes under the session dir (storage meter).
    pub bytes: u64,
}

/// Task registry: new/switch/archive/delete + resume grants.
#[derive(Debug, Default)]
pub struct SessionManager {
    entries: Vec<SessionEntry>,
    active: Option<String>,
}

/// Persisted session-manager schema version (ADR-0016 / P3).
pub const SESSIONS_SNAPSHOT_VERSION: u32 = 1;

/// On-disk shape the owner writes (CLI `.cedian/sessions.json`, app store).
#[derive(Serialize, Deserialize)]
struct Snapshot {
    /// Missing in unversioned files → 0 → rejected.
    #[serde(default)]
    snapshot_version: u32,
    entries: Vec<SessionEntry>,
    active: Option<String>,
}

impl SessionManager {
    /// Serialize for the owner's store, stamped with `snapshot_version`.
    pub fn to_snapshot(&self) -> String {
        serde_json::to_string_pretty(&Snapshot {
            snapshot_version: SESSIONS_SNAPSHOT_VERSION,
            entries: self.entries.clone(),
            active: self.active.clone(),
        })
        .unwrap_or_default()
    }

    /// Restore from [`Self::to_snapshot`] output. Corrupt or other-version
    /// input fails closed ("state too old, re-baseline"), never half-read.
    pub fn from_snapshot(raw: &str) -> Result<Self, String> {
        let snap: Snapshot =
            serde_json::from_str(raw).map_err(|e| format!("corrupt session snapshot: {e}"))?;
        if snap.snapshot_version != SESSIONS_SNAPSHOT_VERSION {
            return Err(format!(
                "session state too old (got v{}, want v{SESSIONS_SNAPSHOT_VERSION}), re-baseline",
                snap.snapshot_version
            ));
        }
        if let Some(active) = &snap.active {
            if !snap.entries.iter().any(|e| &e.task_id == active) {
                return Err(format!(
                    "corrupt session snapshot: active {active:?} not listed"
                ));
            }
        }
        Ok(Self {
            entries: snap.entries,
            active: snap.active,
        })
    }

    /// Empty manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a task (new). Becomes active.
    pub fn add(&mut self, entry: SessionEntry) {
        self.active = Some(entry.task_id.clone());
        if let Some(i) = self.entries.iter().position(|e| e.task_id == entry.task_id) {
            self.entries[i] = entry;
        } else {
            self.entries.push(entry);
        }
    }

    /// Switch active task. `false` when unknown.
    pub fn switch(&mut self, task_id: &str) -> bool {
        if self.entries.iter().any(|e| e.task_id == task_id) {
            self.active = Some(task_id.to_string());
            true
        } else {
            false
        }
    }

    /// Archive (delete) a task. Active falls back to newest remaining.
    pub fn archive(&mut self, task_id: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.task_id != task_id);
        if self.entries.len() == before {
            return false;
        }
        if self.active.as_deref() == Some(task_id) {
            self.active = self.entries.last().map(|e| e.task_id.clone());
        }
        true
    }

    /// Set the resume grant (§76 checkbox). Restart consumes it elsewhere.
    pub fn set_resume(&mut self, task_id: &str, granted: bool) -> bool {
        match self.entries.iter_mut().find(|e| e.task_id == task_id) {
            Some(e) => {
                e.resume_granted = granted;
                true
            }
            None => false,
        }
    }

    /// Active entry, if any.
    pub fn active(&self) -> Option<&SessionEntry> {
        self.active
            .as_ref()
            .and_then(|id| self.entries.iter().find(|e| &e.task_id == id))
    }

    /// All entries, newest last.
    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    /// Total bytes across tasks (storage meter roll-up).
    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.bytes).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> SessionEntry {
        SessionEntry {
            task_id: id.to_string(),
            title: id.to_string(),
            workspace: PathBuf::from("/w"),
            session_dir: PathBuf::from("/s"),
            resume_granted: false,
            bytes: 100,
        }
    }

    #[test]
    fn lifecycle() {
        let mut m = SessionManager::new();
        m.add(entry("a"));
        m.add(entry("b"));
        assert_eq!(m.active().unwrap().task_id, "b");
        assert!(m.switch("a"));
        assert!(!m.switch("zzz"));
        assert!(m.set_resume("a", true));
        assert!(m.archive("a"));
        assert_eq!(m.active().unwrap().task_id, "b");
        assert_eq!(m.total_bytes(), 100);
    }

    #[test]
    fn snapshot_roundtrip_and_fail_closed() {
        let mut m = SessionManager::new();
        m.add(entry("a"));
        let back = SessionManager::from_snapshot(&m.to_snapshot()).unwrap();
        assert_eq!(back.active().map(|e| e.task_id.as_str()), Some("a"));
        assert!(
            SessionManager::from_snapshot(r#"{"entries":[],"active":null}"#)
                .unwrap_err()
                .contains("too old")
        );
        assert!(
            SessionManager::from_snapshot(
                r#"{"snapshot_version":1,"entries":[],"active":"ghost"}"#
            )
            .is_err()
        );
    }
}

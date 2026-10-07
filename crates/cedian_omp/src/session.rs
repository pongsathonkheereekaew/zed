//! Session binding: `workspace ↔ OMP session ↔ task` mapping (plan §75).
//!
//! cedian stores ONLY the mapping + review state — transcripts/sessions belong
//! to OMP. Every persisted record carries `snapshot_version` (herdr discipline):
//! old state fails closed with "state too old, re-baseline", never silently
//! misread. Resume is guarded: blocked-by-default, grants die on restart
//! (plan §76); this module records the guard state, Phase 2 UI enforces it.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
/// Version stamp for all persisted session bindings. Bump on schema change —
/// restores with a different version fail closed.
pub const SNAPSHOT_VERSION: u32 = 1;

/// One workspace↔session↔task mapping record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionBinding {
    /// Schema version — must equal [`SNAPSHOT_VERSION`] to restore.
    pub snapshot_version: u32,
    /// Workspace root this binding belongs to.
    pub workspace: PathBuf,
    /// OMP session directory adopted via `open_session`.
    pub session_dir: PathBuf,
    /// OMP session id (from `open_session` / `get_state`).
    pub session_id: String,
    /// Task label (cedian-side; Phase 2 owns the task model).
    pub task: String,
    /// Guarded-resume state for this binding.
    pub resume: ResumeState,
}

/// Guarded resume: restart never auto-resumes without an explicit user grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResumeState {
    /// Fresh binding — no prior grant.
    New,
    /// User granted resume (checkbox) — valid until restart.
    Granted,
    /// Restart consumed the grant — needs fresh Ask.
    Blocked,
}

impl SessionBinding {
    /// New binding for a workspace + adopted session dir.
    pub fn new(workspace: PathBuf, session_dir: PathBuf, session_id: String, task: String) -> Self {
        Self {
            snapshot_version: SNAPSHOT_VERSION,
            workspace,
            session_dir,
            session_id,
            task,
            resume: ResumeState::New,
        }
    }

    /// Validate on restore: version match + absolute paths. Fails closed.
    pub fn validate(&self) -> Result<(), String> {
        if self.snapshot_version != SNAPSHOT_VERSION {
            return Err(format!(
                "state too old, re-baseline (got v{}, want v{SNAPSHOT_VERSION})",
                self.snapshot_version
            ));
        }
        if !self.workspace.is_absolute() || !self.session_dir.is_absolute() {
            return Err("binding paths must be absolute".to_string());
        }
        if self.session_id.is_empty() {
            return Err("binding has no session id".to_string());
        }
        Ok(())
    }
}

/// Version-drift error for binding restore (fails closed, re-baseline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotVersionMismatch {
    /// Version found in the stored record.
    pub got: u32,
    /// Version this build understands.
    pub want: u32,
}

impl std::fmt::Display for SnapshotVersionMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "state too old, re-baseline (got v{}, want v{})",
            self.got, self.want
        )
    }
}

impl std::error::Error for SnapshotVersionMismatch {}

/// Validate a restored binding: version match (typed) + absolute paths.
/// Fails closed — never silently misread old state.
pub fn validate_binding(binding: &SessionBinding) -> Result<(), SnapshotVersionMismatch> {
    if binding.snapshot_version != SNAPSHOT_VERSION {
        return Err(SnapshotVersionMismatch {
            got: binding.snapshot_version,
            want: SNAPSHOT_VERSION,
        });
    }
    binding.validate().map_err(|_| SnapshotVersionMismatch {
        got: binding.snapshot_version,
        want: SNAPSHOT_VERSION,
    })
}

impl SessionBinding {
    /// Restart consumes any grant: `Granted → Blocked`, rest unchanged.
    pub fn on_restart(&mut self) {
        if self.resume == ResumeState::Granted {
            self.resume = ResumeState::Blocked;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_validate() {
        let b = SessionBinding::new(
            PathBuf::from("/w"),
            PathBuf::from("/s"),
            "sess-1".to_string(),
            "t".to_string(),
        );
        b.validate().unwrap();
        let json = serde_json::to_string(&b).unwrap();
        let back: SessionBinding = serde_json::from_str(&json).unwrap();
        back.validate().unwrap();
    }

    #[test]
    fn stale_version_fails_closed() {
        let mut b = SessionBinding::new(
            PathBuf::from("/w"),
            PathBuf::from("/s"),
            "sess-1".to_string(),
            "t".to_string(),
        );
        b.snapshot_version = 0;
        assert!(b.validate().is_err());
    }

    #[test]
    fn restart_consumes_grant() {
        let mut b = SessionBinding::new(
            PathBuf::from("/w"),
            PathBuf::from("/s"),
            "sess-1".to_string(),
            "t".to_string(),
        );
        b.resume = ResumeState::Granted;
        b.on_restart();
        assert_eq!(b.resume, ResumeState::Blocked);
    }
}

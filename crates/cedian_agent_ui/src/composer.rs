//! Composer model: draft, mode, and queue chips (plan Phase 2 `composer`).
//!
//! The composer owns the unsent draft + send mode. Queue chips (steering /
//! follow-up) mirror `get_state.queuedMessages` — rendered from snapshots,
//! never tracked independently (wire contract). Drafts persist per task
//! (debounced, version-stamped, die with task — §76); persistence itself is a
//! Phase 2.5 shell concern, this model exposes the versioned snapshot.

use serde::{Deserialize, Serialize};

/// Send mode: new turn vs queue into the live run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ComposerMode {
    /// Fresh prompt (idle session) — plain `prompt`.
    #[default]
    New,
    /// Queue as steering (interrupt path) while streaming.
    Steer,
    /// Queue as follow-up (post-turn path) while streaming.
    FollowUp,
}

/// Versioned draft snapshot (§76: debounced persist, version-stamped).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftSnapshot {
    /// Monotonic version per task; the store keeps the newest.
    pub version: u64,
    pub text: String,
    pub mode: ComposerMode,
}

/// Composer state for one task.
#[derive(Debug, Default)]
pub struct Composer {
    text: String,
    mode: ComposerMode,
    version: u64,
    busy: bool,
}

impl Composer {
    /// Empty composer in `New` mode.
    pub fn new() -> Self {
        Self::default()
    }

    /// Edit the draft; bumps the persist version.
    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.version += 1;
    }

    /// Switch send mode (e.g. session started streaming → `Steer`).
    pub fn set_mode(&mut self, mode: ComposerMode) {
        self.mode = mode;
    }

    /// Mark the session busy (streaming) or idle. Busy + `New` mode is a
    /// caller error — the owner must pick `Steer`/`FollowUp` first (wire rule:
    /// `prompt` during streaming requires `streamingBehavior`).
    pub fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
    }

    /// Take the draft for send. Fails when busy in `New` mode, or when the
    /// draft is empty. Clears the draft on success (version kept).
    pub fn take_for_send(&mut self) -> Result<(String, ComposerMode), ComposerError> {
        if self.text.trim().is_empty() {
            return Err(ComposerError::Empty);
        }
        if self.busy && self.mode == ComposerMode::New {
            return Err(ComposerError::NeedsQueueMode);
        }
        let text = std::mem::take(&mut self.text);
        self.version += 1;
        Ok((text, self.mode))
    }

    /// Versioned snapshot for the debounced persist (§76).
    pub fn snapshot(&self) -> DraftSnapshot {
        DraftSnapshot {
            version: self.version,
            text: self.text.clone(),
            mode: self.mode,
        }
    }

    /// Restore a snapshot (newest version wins — caller compares).
    pub fn restore(&mut self, snapshot: &DraftSnapshot) {
        if snapshot.version >= self.version {
            self.text = snapshot.text.clone();
            self.mode = snapshot.mode;
            self.version = snapshot.version;
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn mode(&self) -> ComposerMode {
        self.mode
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }
}

/// Composer send errors (caller-visible, rendered inline under the composer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerError {
    /// Nothing to send.
    Empty,
    /// Session is streaming — pick steer/follow-up first.
    NeedsQueueMode,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_clears_draft() {
        let mut c = Composer::new();
        c.set_text("hi");
        let (text, mode) = c.take_for_send().unwrap();
        assert_eq!(text, "hi");
        assert_eq!(mode, ComposerMode::New);
        assert!(c.text().is_empty());
    }

    #[test]
    fn busy_new_mode_rejected() {
        let mut c = Composer::new();
        c.set_text("hi");
        c.set_busy(true);
        assert_eq!(c.take_for_send(), Err(ComposerError::NeedsQueueMode));
        c.set_mode(ComposerMode::Steer);
        assert!(c.take_for_send().is_ok());
    }

    #[test]
    fn snapshot_restore_newest_wins() {
        let mut c = Composer::new();
        c.set_text("v1");
        let snap = c.snapshot();
        c.set_text("v2");
        c.restore(&snap);
        assert_eq!(c.text(), "v2");
    }
}

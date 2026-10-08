//! Hunk identity and persisted resolutions (the panel's review keys them).

use crate::HunkStatus;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Stable hunk identity: survives rebuilds that shift indices.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HunkKey {
    pub before_start: usize,
    pub before_count: usize,
    /// Exact after-side lines (joined with `\n`).
    pub after_text: String,
}

/// One persisted user resolution / manual status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusRecord {
    pub path: PathBuf,
    pub key: HunkKey,
    pub status: HunkStatus,
}

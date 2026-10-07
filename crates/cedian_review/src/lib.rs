//! `cedian_review`: edit provenance + review changes (headless).
//!
//! Plan §§15–20: task baseline (NOT git HEAD) + cedian-owned `AgentEdit`
//! store (survives OMP compaction) + hunk accept/reject semantics + feedback
//! payloads. Thin projection over buffer text — no parallel diff engine; Zed
//! multibuffer/excerpt binding lands with the fork.
//!
//! State precedence (§18 R3, strictly ordered):
//! `INTERRUPTED` (crash orphan) > `UNATTRIBUTED` (no tool_call_id) > `STALE`
//! (user edited after agent). One badge per hunk — the highest present.
//! Allowed transitions only: `INTERRUPTED → UNATTRIBUTED` (reconciled) →
//! `STALE`/`accepted`/`rejected` (user resolved). No silent demotion.

pub mod baseline;
pub mod diff;
pub mod findings;
pub mod provenance;
pub mod revert;
pub mod tracker;

pub use baseline::{Baseline, BaselineError};
pub use diff::{FileDiff, Hunk, HunkStatus, line_diff};
pub use findings::{AttachedFinding, FindingSeverity, ReviewFeedback, ReviewFinding, attach};
pub use provenance::{AgentEdit, ProvenanceStore};
pub use revert::{RevertFile, revert_file};
pub use tracker::{HunkKey, ReviewTracker, StatusRecord, TrackerError};

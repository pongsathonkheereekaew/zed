//! `cedian_review`: hunk shapes, review findings and feedback payloads.
//!
//! The review itself runs on Zed buffers in `cedian_panel`; this crate holds
//! the shapes it and the headless reviewer share.
//!
//! State precedence (§18 R3, strictly ordered):
//! `INTERRUPTED` (crash orphan) > `UNATTRIBUTED` (no tool_call_id) > `STALE`
//! (user edited after agent). One badge per hunk — the highest present.

pub mod diff;
pub mod findings;
pub mod resolution;

pub use diff::{FileDiff, Hunk, HunkStatus, line_diff};
pub use findings::{AttachedFinding, FindingSeverity, ReviewFeedback, ReviewFinding, attach};
pub use resolution::{HunkKey, StatusRecord};

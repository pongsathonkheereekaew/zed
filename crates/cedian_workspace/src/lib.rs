//! `cedian_workspace`: editor-native edit surface (headless).
//!
//! Plan §§10–14: OMP owns its `edit` tool end-to-end (no backend seam); cedian
//! implements the HOST side — `WorkspaceHost` trait, `cedian://` virtual files,
//! host tools for buffer transactions. In-memory backend now; Zed binding
//! (`clock::Global`, `text::Transaction`, `History::start/end/push`) swaps in
//! mechanically with the fork (same trait shape as plan §12).
//!
//! V1 unsaved strategy (§13): Agent Sync = ON — dirty buffers save before the
//! turn, versions recorded, idempotent turn start. Tracked as tech debt with
//! this crate as owner; overlay-filesystem semantics replace it once the
//! `clock::Global` baseline (§16) is solid.

pub mod ambient;
pub mod buffer;
pub mod host;
pub mod service;
pub mod uri;

pub use ambient::{AmbientSnapshot, capture_ambient, render_snapshot};
pub use buffer::{ApplyEditResult, BufferStore, TextEdit, parse_version_token, version_token};
pub use host::{APPLY_EDIT_TOOL, Diagnostic, DiagnosticSeverity, HostTools, WorkspaceHost};
pub use service::{HostService, ServiceId};
pub use uri::{CedianUri, UriKind, parse_cedian_uri};

//! `cedian_workspace`: the host side of the editor (plan §§10–14). OMP owns
//! its `edit` tool end-to-end (ADR-0057 decision 6); cedian serves the
//! `WorkspaceHost` trait and `cedian://` virtual files over the texts the
//! app (Zed's buffers) or the headless CLI (the disk) hands it.

pub mod ambient;
pub mod host;
pub mod service;
pub mod uri;

pub use ambient::{AmbientSnapshot, capture_ambient, render_snapshot};
pub use host::{Diagnostic, DiagnosticSeverity, HostTools, WorkspaceHost};
pub use service::{HostService, ServiceId};
pub use uri::{CedianUri, UriKind, parse_cedian_uri};

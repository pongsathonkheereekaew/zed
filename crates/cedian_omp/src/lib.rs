//! `cedian_omp`: process + protocol boundary to the bundled OMP sidecar.
//!
//! Plan §§4–6: `cedian.app` spawns `omp --mode rpc-ui` over stdio, negotiates
//! RPC v2 through the vendored upstream client (`vendor/omp-rpc`), and fans
//! session events out through [`EventRouter`]. Blocking client calls live on
//! caller threads off-UI — never the UI thread (plan §5).
#![allow(
    clippy::disallowed_methods,
    reason = "headless, synchronous process control (OMP, LSP, DAP, Chrome, git, sandbox-exec): Zed's async spawn helpers do not apply"
)]

pub mod errors;
pub mod event_router;
pub mod headless_ui;
pub mod runtime;
pub mod sandbox;
pub mod session;
pub mod spawn_profile;

pub use errors::OmpError;
pub use event_router::{
    DeltaKind, EventRouter, FinishedToolCall, LogEntry, PromptStatus, RouterEvent,
};
pub use headless_ui::{GateDecision, Refusal, headless_answer};
pub use runtime::{
    OmpBinary, OmpRuntime, RuntimeConfig, RuntimeControl, RuntimeState, image_content, last_text,
};
pub use session::{
    ResumeState, SNAPSHOT_VERSION, SessionBinding, SnapshotVersionMismatch, validate_binding,
};
pub use spawn_profile::{
    ApprovalMode, Approvals, BashRule, SpawnPlan, SpawnPolicy, SpawnProfile, ToolPolicy,
    omp_config_get, resolve_on_path, scrub_env,
};

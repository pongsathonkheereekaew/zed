//! `cedian_browser`: cedian-owned headless Chromium over CDP (S4).
//!
//! Plan refs Phases 9 (shared browser) + 11 (browser as evidence provider)
//! and §29 R4 (evidence carries CDP frame id; stale-frame requires
//! re-capture).
//!
//! Notes: R1 user-input-preempts is a GPUI concern, deferred to the S9
//! binding. Blocking/sync API like `cedian_lsp`/`cedian_dap`; never spawn
//! threads inside gate evaluation. S9 handoff: this crate dies at the GPUI
//! binding, which takes over process ownership.
#![allow(
    clippy::disallowed_methods,
    reason = "headless, synchronous process control (OMP, LSP, DAP, Chrome, git, sandbox-exec): Zed's async spawn helpers do not apply"
)]

pub mod cdp;
pub mod process;
pub mod session;

pub use cdp::{CdpClient, CdpError, CdpEvent};
pub use process::{BrowserError, BrowserProcess};
pub use session::{BrowserSession, DomSnapshot, Shot, is_stale};

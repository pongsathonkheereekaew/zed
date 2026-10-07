//! `cedian_agent`: headless agent model — one task = one OMP session.
//!
//! Plan §7 layout (`task.rs`, `thread.rs`, `state.rs`): pure state machines
//! fed by `EventRouter` classifications. No GPUI, no I/O, no threads here —
//! the owner pumps router events in and reads view snapshots out. Panel
//! tri-state (`working | blocked | idle`, herdr §75) derives from this model.

pub mod state;
pub mod task;
pub mod thread;

pub use state::{AgentStatus, ModelState, ThinkingState};
pub use task::{Task, TaskId};
pub use thread::{Thread, ThreadEvent, ToolCallState, ToolCallStatus};

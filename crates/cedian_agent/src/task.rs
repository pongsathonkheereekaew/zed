//! `Task`: one unit of agent work = one OMP session + one thread + status.
//!
//! Plan §§75–76: cedian stores the `workspace ↔ OMP session ↔ task` mapping
//! (via `SessionBinding`); the task adds lifecycle, attention, and guarded
//! resume on top. OMP owns the transcript — the task never duplicates it.

use crate::{
    Thread,
    state::{AgentStatus, ModelState, ThinkingState},
};
use cedian_omp::{SessionBinding, SnapshotVersionMismatch, validate_binding};
use std::path::PathBuf;

/// Opaque task identifier (cedian-side; `task-` + counter per process).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaskId(pub String);

/// One unit of agent work.
#[derive(Debug)]
pub struct Task {
    id: TaskId,
    title: String,
    workspace: PathBuf,
    binding: Option<SessionBinding>,
    thread: Thread,
    status: AgentStatus,
    model: ModelState,
    thinking: ThinkingState,
    /// herdr §75: bool reset on task-open (no attention counter v1).
    needs_attention: bool,
}

impl Task {
    /// New task for a workspace. No session yet — `bind` after `open_session`.
    pub fn new(id: TaskId, title: &str, workspace: PathBuf) -> Self {
        Self {
            id,
            title: title.to_string(),
            workspace,
            binding: None,
            thread: Thread::new(),
            status: AgentStatus::Idle,
            model: ModelState::default(),
            thinking: ThinkingState::default(),
            needs_attention: false,
        }
    }

    /// Attach the workspace↔session binding after `open_session` (validated,
    /// fails closed on version drift).
    pub fn bind(&mut self, binding: SessionBinding) -> Result<(), SnapshotVersionMismatch> {
        validate_binding(&binding)?;
        self.binding = Some(binding);
        Ok(())
    }

    /// Turn started → `working`.
    pub fn on_turn_start(&mut self) {
        self.status = AgentStatus::Working;
    }

    /// Turn yielded terminally → `idle`. Non-terminal (continuation pending) →
    /// stays `working`.
    pub fn on_turn_end(&mut self, is_terminal: bool) {
        if is_terminal {
            self.status = AgentStatus::Idle;
        }
    }

    /// Pending ask/abstain → `blocked` + attention (surfaces unopened).
    pub fn on_blocked(&mut self) {
        self.status = AgentStatus::Blocked;
        self.needs_attention = true;
    }

    /// Opening the task clears attention (but not `blocked` — the ask still
    /// needs an answer).
    pub fn on_open(&mut self) {
        self.needs_attention = false;
    }

    /// Refresh model/thinking display state from a `get_state` snapshot.
    pub fn set_model_state(&mut self, model: ModelState, thinking: ThinkingState) {
        self.model = model;
        self.thinking = thinking;
    }

    pub fn id(&self) -> &TaskId {
        &self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn status(&self) -> AgentStatus {
        self.status
    }

    pub fn needs_attention(&self) -> bool {
        self.needs_attention
    }

    pub fn thread(&self) -> &Thread {
        &self.thread
    }

    pub fn thread_mut(&mut self) -> &mut Thread {
        &mut self.thread
    }

    pub fn binding(&self) -> Option<&SessionBinding> {
        self.binding.as_ref()
    }

    pub fn workspace(&self) -> &PathBuf {
        &self.workspace
    }

    pub fn model(&self) -> &ModelState {
        &self.model
    }

    pub fn thinking(&self) -> ThinkingState {
        self.thinking
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> Task {
        Task::new(TaskId("task-1".to_string()), "t", PathBuf::from("/w"))
    }

    #[test]
    fn status_transitions() {
        let mut t = task();
        assert_eq!(t.status(), AgentStatus::Idle);
        t.on_turn_start();
        assert_eq!(t.status(), AgentStatus::Working);
        t.on_turn_end(false);
        assert_eq!(t.status(), AgentStatus::Working);
        t.on_turn_end(true);
        assert_eq!(t.status(), AgentStatus::Idle);
    }

    #[test]
    fn blocked_sets_attention_open_clears() {
        let mut t = task();
        t.on_blocked();
        assert_eq!(t.status(), AgentStatus::Blocked);
        assert!(t.needs_attention());
        t.on_open();
        assert!(!t.needs_attention());
        assert_eq!(t.status(), AgentStatus::Blocked);
    }
}

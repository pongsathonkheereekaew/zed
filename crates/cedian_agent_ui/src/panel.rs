//! Panel model: task list + active task view (plan Phase 2 `OMP thread`,
//! `model state`, `thinking state`).
//!
//! The panel owns tasks (one thread + composer + ask-dialogs each), routes
//! router classifications into the active thread, and projects render
//! snapshots. Session-manager surface (new/switch/archive/delete — §2.5)
//! builds on `new_task`/`switch`/`archive` here.

use crate::{AskDialog, Composer, TextDeltaBuffer};
use cedian_agent::{AgentStatus, Task, TaskId};
use cedian_omp::RouterEvent;
use std::collections::HashMap;

/// Render snapshot of one task for the panel view.
#[derive(Debug, Clone)]
pub struct PanelTask {
    pub id: String,
    pub title: String,
    pub status: AgentStatus,
    pub needs_attention: bool,
    pub active: bool,
}

/// The agent panel: task registry + router fan-in.
#[derive(Debug, Default)]
pub struct Panel {
    tasks: HashMap<String, Task>,
    order: Vec<String>,
    active: Option<String>,
    next_id: u64,
    composers: HashMap<String, Composer>,
    buffers: HashMap<String, TextDeltaBuffer>,
    asks: HashMap<String, Vec<AskDialog>>,
}

impl Panel {
    /// Empty panel.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a task for a workspace; becomes active. Returns its id.
    pub fn new_task(&mut self, title: &str, workspace: std::path::PathBuf) -> TaskId {
        self.next_id += 1;
        let id = TaskId(format!("task-{}", self.next_id));
        self.order.push(id.0.clone());
        self.tasks
            .insert(id.0.clone(), Task::new(id.clone(), title, workspace));
        self.composers.insert(id.0.clone(), Composer::new());
        self.buffers.insert(id.0.clone(), TextDeltaBuffer::new());
        self.active = Some(id.0.clone());
        id
    }

    /// Switch the active task (clears its attention).
    pub fn switch(&mut self, id: &TaskId) -> bool {
        if !self.tasks.contains_key(&id.0) {
            return false;
        }
        self.active = Some(id.0.clone());
        if let Some(task) = self.tasks.get_mut(&id.0) {
            task.on_open();
        }
        true
    }

    /// Archive (drop) a task: composer drafts die with it (§76). Active task
    /// falls back to the newest remaining.
    pub fn archive(&mut self, id: &TaskId) -> bool {
        if self.tasks.remove(&id.0).is_none() {
            return false;
        }
        self.composers.remove(&id.0);
        self.buffers.remove(&id.0);
        self.asks.remove(&id.0);
        self.order.retain(|x| x != &id.0);
        if self.active.as_ref() == Some(&id.0) {
            self.active = self.order.last().cloned();
        }
        true
    }

    /// Route one router classification into the ACTIVE task's thread. Turn
    /// boundaries also flip task status (working/idle).
    pub fn dispatch(&mut self, event: &RouterEvent) {
        let Some(active) = self.active.clone() else {
            return;
        };
        let Some(task) = self.tasks.get_mut(&active) else {
            return;
        };
        match event {
            RouterEvent::AgentStart => task.on_turn_start(),
            RouterEvent::AgentEnd { is_terminal, .. } => task.on_turn_end(*is_terminal),
            _ => {}
        }
        task.thread_mut().apply(event);
    }

    /// Disconnect path: discard partial buffers, interrupt orphaned cards.
    pub fn on_disconnect(&mut self, interrupted: &[String]) {
        for (id, buffer) in self.buffers.iter_mut() {
            buffer.discard_all();
            if let Some(task) = self.tasks.get_mut(id) {
                task.thread_mut().on_disconnect(interrupted);
            }
        }
    }

    /// Register a pending ask dialog on the active task → `blocked` + attention.
    pub fn push_ask(&mut self, dialog: AskDialog) {
        let Some(active) = self.active.clone() else {
            return;
        };
        if let Some(task) = self.tasks.get_mut(&active) {
            task.on_blocked();
        }
        self.asks.entry(active).or_default().push(dialog);
    }

    /// Task list snapshot (panel sidebar).
    pub fn task_list(&self) -> Vec<PanelTask> {
        self.order
            .iter()
            .filter_map(|id| {
                self.tasks.get(id).map(|t| PanelTask {
                    id: id.clone(),
                    title: t.title().to_string(),
                    status: t.status(),
                    needs_attention: t.needs_attention(),
                    active: self.active.as_ref() == Some(id),
                })
            })
            .collect()
    }

    pub fn get(&self, id: &TaskId) -> Option<&Task> {
        self.tasks.get(&id.0)
    }

    pub fn get_mut(&mut self, id: &TaskId) -> Option<&mut Task> {
        self.tasks.get_mut(&id.0)
    }

    pub fn active_id(&self) -> Option<TaskId> {
        self.active.clone().map(TaskId)
    }

    pub fn composer_mut(&mut self, id: &TaskId) -> Option<&mut Composer> {
        self.composers.get_mut(&id.0)
    }

    pub fn buffer_mut(&mut self, id: &TaskId) -> Option<&mut TextDeltaBuffer> {
        self.buffers.get_mut(&id.0)
    }

    pub fn asks(&self, id: &TaskId) -> &[AskDialog] {
        self.asks.get(&id.0).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_lifecycle_and_attention() {
        let mut p = Panel::new();
        let id = p.new_task("t", "/w".into());
        assert_eq!(p.task_list().len(), 1);
        p.dispatch(&RouterEvent::AgentStart);
        assert_eq!(p.get(&id).unwrap().status(), AgentStatus::Working);
        p.dispatch(&RouterEvent::AgentEnd {
            yielded: true,
            is_terminal: true,
        });
        assert_eq!(p.get(&id).unwrap().status(), AgentStatus::Idle);
    }

    #[test]
    fn archive_drops_draft_and_falls_back() {
        let mut p = Panel::new();
        let a = p.new_task("a", "/w".into());
        let b = p.new_task("b", "/w".into());
        assert_eq!(p.active_id(), Some(b.clone()));
        assert!(p.archive(&b));
        assert_eq!(p.active_id(), Some(a));
        assert!(!p.archive(&b));
    }
}

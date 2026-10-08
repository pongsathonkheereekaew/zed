//! The subagent tree (ADR-0050): each OMP subagent, keyed by its id, under
//! the `task` tool call that started it (`parentToolCallId`), with the
//! status OMP last reported. Pure: fed [`RouterEvent`]s, no I/O.

use cedian_omp::{RouterEvent, SubagentStatus};

/// One subagent as the panel shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentRow {
    pub id: String,
    pub agent: String,
    pub description: String,
    pub parent_tool_call_id: Option<String>,
    pub status: SubagentStatus,
}

/// Every subagent this session has seen, in the order each first appeared.
#[derive(Debug, Default)]
pub struct SubagentTree {
    rows: Vec<SubagentRow>,
}

impl SubagentTree {
    pub fn apply(&mut self, event: &RouterEvent) {
        let (id, agent, parent, description, status) = match event {
            RouterEvent::SubagentLifecycle {
                id,
                agent,
                status,
                parent_tool_call_id,
                description,
            } => (id, agent, parent_tool_call_id, description, Some(*status)),
            RouterEvent::SubagentProgress {
                id,
                agent,
                parent_tool_call_id,
                description,
            } => (id, agent, parent_tool_call_id, description, None),
            _ => return,
        };
        let index = match self.rows.iter().position(|row| row.id == *id) {
            Some(index) => index,
            None => {
                self.rows.push(SubagentRow {
                    id: id.clone(),
                    agent: agent.clone(),
                    description: String::new(),
                    parent_tool_call_id: None,
                    status: SubagentStatus::Running,
                });
                self.rows.len() - 1
            }
        };
        let row = &mut self.rows[index];
        if row.parent_tool_call_id.is_none() {
            row.parent_tool_call_id = parent.clone();
        }
        if let Some(description) = description.as_ref().filter(|d| !d.is_empty()) {
            row.description = description.clone();
        }
        // An ended subagent stays ended: a late progress frame does not revive it.
        if let (SubagentStatus::Running, Some(status)) = (row.status, status) {
            row.status = status;
        }
    }

    /// The subagents started by tool call `tool_call_id`.
    pub fn under(&self, tool_call_id: &str) -> Vec<&SubagentRow> {
        self.rows
            .iter()
            .filter(|row| row.parent_tool_call_id.as_deref() == Some(tool_call_id))
            .collect()
    }

    pub fn rows(&self) -> &[SubagentRow] {
        &self.rows
    }

    pub fn get(&self, id: &str) -> Option<&SubagentRow> {
        self.rows.iter().find(|row| row.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lifecycle(id: &str, status: SubagentStatus) -> RouterEvent {
        RouterEvent::SubagentLifecycle {
            id: id.into(),
            agent: "explore".into(),
            status,
            parent_tool_call_id: Some("call-task".into()),
            description: Some("map".into()),
        }
    }

    fn progress(id: &str, description: &str) -> RouterEvent {
        RouterEvent::SubagentProgress {
            id: id.into(),
            agent: "explore".into(),
            parent_tool_call_id: Some("call-task".into()),
            description: Some(description.into()),
        }
    }

    #[test]
    fn rows_sit_under_their_task_call_and_an_ended_one_stays_ended() {
        let mut tree = SubagentTree::default();
        tree.apply(&lifecycle("sa-1", SubagentStatus::Running));
        tree.apply(&progress("sa-1", "reading"));
        tree.apply(&lifecycle("sa-2", SubagentStatus::Running));
        tree.apply(&lifecycle("sa-1", SubagentStatus::Completed));
        tree.apply(&progress("sa-1", "late"));
        let under: Vec<_> = tree
            .under("call-task")
            .into_iter()
            .map(|r| (r.id.as_str(), r.status))
            .collect();
        assert_eq!(
            under,
            [
                ("sa-1", SubagentStatus::Completed),
                ("sa-2", SubagentStatus::Running)
            ]
        );
        assert_eq!(tree.get("sa-1").unwrap().description, "late");
        assert!(tree.under("other").is_empty());
    }
}

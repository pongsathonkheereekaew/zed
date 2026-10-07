//! TaskProfile (§47): what the task IS — kind, complexity, risk, surfaces,
//! constraints, acceptance criteria. Pure data; serializes to workflow.json.

use serde::{Deserialize, Serialize};

/// Task kind selects the default playbook (§49 V1 set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Investigation,
    BugFix,
    Feature,
    Refactor,
    Performance,
    Prototype,
}

/// Complexity bands (estimate only — gates, not phases, enforce quality).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Complexity {
    Trivial,
    Small,
    Medium,
    Large,
}

/// Risk drives the conditional review phase (§56): low skips review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    Medium,
    High,
}

/// Surfaces the task touches (§47).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    Library,
    Cli,
    Web,
    Desktop,
    Ios,
    Android,
    Api,
    Database,
}

/// One acceptance criterion (§56 feature playbook: all must hold).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceCriterion {
    pub id: String,
    pub description: String,
}

/// TaskProfile: the task under workflow control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskProfile {
    pub title: String,
    pub kind: TaskKind,
    pub complexity: Complexity,
    pub risk: Risk,
    pub surfaces: Vec<Surface>,
    pub constraints: Vec<String>,
    pub acceptance_criteria: Vec<AcceptanceCriterion>,
}

impl TaskProfile {
    /// Minimal profile: kind + title + low risk, no surfaces/criteria.
    /// Callers add risk/surfaces/criteria; gates read what is present.
    pub fn new(title: impl Into<String>, kind: TaskKind) -> Self {
        Self {
            title: title.into(),
            kind,
            complexity: Complexity::Small,
            risk: Risk::Low,
            surfaces: Vec::new(),
            constraints: Vec::new(),
            acceptance_criteria: Vec::new(),
        }
    }

    /// Playbook id for this kind (§49 V1 set).
    pub fn playbook_id(&self) -> &'static str {
        match self.kind {
            TaskKind::Investigation => "investigation",
            TaskKind::BugFix => "bug_fix",
            TaskKind::Feature => "feature",
            TaskKind::Refactor => "refactor",
            TaskKind::Performance | TaskKind::Prototype => "generic",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_selects_playbook() {
        assert_eq!(
            TaskProfile::new("t", TaskKind::BugFix).playbook_id(),
            "bug_fix"
        );
        assert_eq!(
            TaskProfile::new("t", TaskKind::Prototype).playbook_id(),
            "generic"
        );
    }
}

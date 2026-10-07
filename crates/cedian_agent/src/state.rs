//! Agent lifecycle + model/thinking state (plan Phase 2: `model state`,
//! `thinking state`). Derived from router events + `get_state` snapshots —
//! never parsed from terminal text.

use omp_rpc::{SessionState, ThinkingLevel};

/// Panel tri-state (herdr §75): running turn → `working`, pending
/// ask/abstain → `blocked`, else `idle`. A `blocked` task surfaces WITHOUT
/// opening the panel; `needs_attention` resets on task-open (no counter v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentStatus {
    #[default]
    Idle,
    Working,
    Blocked,
}

/// Which model serves this task. OMP owns the catalog; cedian renders it
/// (Phase 2.5 model picker mutates via `set_model`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelState {
    /// `provider/id` pair, e.g. `opencode-go/muse-spark-1.3-contributor`.
    pub provider: String,
    pub id: String,
    pub name: String,
}

/// Thinking level as the live model reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingState {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl From<ThinkingLevel> for ThinkingState {
    fn from(level: ThinkingLevel) -> Self {
        match level {
            ThinkingLevel::Off => Self::Off,
            ThinkingLevel::Minimal => Self::Minimal,
            ThinkingLevel::Low => Self::Low,
            ThinkingLevel::Medium => Self::Medium,
            ThinkingLevel::High => Self::High,
            ThinkingLevel::Xhigh => Self::XHigh,
            ThinkingLevel::Max => Self::Max,
            _ => Self::Off,
        }
    }
}

/// Refresh model/thinking state from a `get_state` snapshot.
pub fn model_from_state(state: &SessionState) -> (ModelState, ThinkingState) {
    let model = state.model.as_ref().map(|m| ModelState {
        provider: m.provider.clone(),
        id: m.id.clone(),
        name: m.name.clone(),
    });
    let thinking = state.thinking_level.as_ref().copied().map(Into::into);
    (model.unwrap_or_default(), thinking.unwrap_or_default())
}

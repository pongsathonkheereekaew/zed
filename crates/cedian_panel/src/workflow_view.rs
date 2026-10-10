//! The workflow as the panel shows it (ADR-0055): phases, gates with their
//! reasons, evidence with its outcome and freshness, and the last claims
//! ledger. Built off the UI thread from `workflow.json` and the workspace
//! hashed now; inconclusive evidence is shown as inconclusive, never as a
//! pass (ADR-0024).

use cedian_workflow::{
    CurrentState, GateStatus, Outcome, PhaseStatus, WorkflowState, WorkflowStatus,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowView {
    pub title: String,
    pub status: WorkflowStatus,
    /// `(phase, mark)`: ✓ passed, ● the current one, ✗ failed, ‖ blocked,
    /// – skipped, ○ to come.
    pub phases: Vec<(String, char)>,
    pub gates: Vec<GateLine>,
    pub evidence: Vec<String>,
    /// The last `cedian_complete`: accepted or not, and each claim with its
    /// label, evidence and flag.
    pub claims: Option<String>,
    /// The files an edit must touch to change what this view shows: every
    /// file the evidence binds, or all when some binds the whole tree.
    pub covers: Covers,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Covers {
    pub tree: bool,
    pub files: std::collections::BTreeSet<String>,
}

impl Covers {
    pub fn covers(&self, path: &str) -> bool {
        self.tree || self.files.contains(path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateLine {
    pub id: String,
    pub required: bool,
    pub status: GateStatus,
    pub reason: String,
}

impl WorkflowView {
    pub fn new(state: &WorkflowState, current: &CurrentState) -> Self {
        let phases = state
            .phases
            .iter()
            .map(|p| {
                let mark = match p.status {
                    PhaseStatus::Passed => '✓',
                    PhaseStatus::Running => '●',
                    PhaseStatus::Failed => '✗',
                    PhaseStatus::Blocked => '‖',
                    PhaseStatus::Skipped => '–',
                    PhaseStatus::Pending => '○',
                };
                (p.id.clone(), mark)
            })
            .collect();
        let gates = state
            .all_gates(current)
            .into_iter()
            .map(|(id, result)| GateLine {
                required: state
                    .playbook
                    .gates
                    .iter()
                    .any(|g| g.id == id && g.required),
                status: result.status,
                reason: if result.unverified_origin {
                    format!("{} (unverified-origin)", result.reason)
                } else {
                    result.reason
                },
                id,
            })
            .collect();
        let mut items = state.evidence_list();
        items.sort_by_key(|e| {
            e.id.trim_start_matches('e')
                .parse::<u64>()
                .unwrap_or(u64::MAX)
        });
        let mut covers = Covers::default();
        for e in &items {
            match &e.code_state {
                Some(cedian_workflow::CodeState::Files(files)) => {
                    covers.files.extend(files.keys().cloned())
                }
                Some(cedian_workflow::CodeState::Tree(_)) => covers.tree = true,
                None => {}
            }
        }
        let evidence = items
            .iter()
            .map(|e| {
                let outcome = match e.outcome {
                    Outcome::Pass => "pass",
                    Outcome::Fail => "fail",
                    Outcome::Inconclusive => "inconclusive",
                };
                let origin = if e.is_attributed() {
                    ""
                } else {
                    " · unattributed"
                };
                let fresh = e
                    .stale_reason(current)
                    .map(|why| format!(" · stale: {why}"))
                    .unwrap_or_default();
                format!(
                    "{} [{}] {outcome}: {}{origin}{fresh}",
                    e.id,
                    e.for_gates.join(","),
                    e.summary
                )
            })
            .collect();
        let claims = state.last_completion.as_ref().map(|c| {
            let verdict = if c.accepted { "accepted" } else { "refused" };
            format!(
                "last completion: {verdict}{}",
                cedian_workflow::ledger_lines(&c.claims)
            )
        });
        Self {
            title: format!("{} · {:?}", state.task.title, state.task.kind),
            status: state.status,
            phases,
            gates,
            evidence,
            claims,
            covers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cedian_workflow::{Evidence, EvidenceKind, TaskKind, TaskProfile};

    /// ADR-0024: evidence that could not run says so, and never passes the
    /// gate it was reported for.
    #[test]
    fn inconclusive_evidence_is_never_shown_as_a_pass() {
        let mut state = WorkflowState::start(TaskProfile::new("t", TaskKind::BugFix)).unwrap();
        let current = CurrentState::from_files([("a.rs".to_string(), b"x".as_slice())]);
        let mut item = Evidence::attributed(
            "e1",
            EvidenceKind::Test,
            &["verify"],
            "cargo test did not start",
            Outcome::Inconclusive,
            "panel",
            "call-1",
        );
        item.code_state = Some(current.bind(&[]));
        state.attach(item).unwrap();
        let view = WorkflowView::new(&state, &current);
        assert_eq!(
            view.evidence,
            vec!["e1 [verify] inconclusive: cargo test did not start".to_string()]
        );
        let verify = view.gates.iter().find(|g| g.id == "verify").unwrap();
        assert_ne!(verify.status, GateStatus::Passed, "{verify:?}");
        assert!(!view.evidence[0].contains("pass"));
    }
}

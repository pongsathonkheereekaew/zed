//! Playbooks (§49): ordered phases + per-playbook gate policies (§56).
//!
//! V1 set only — investigation, bug_fix, feature, refactor, generic. No
//! second engine: a playbook is DATA (phases + gates); `WorkflowState` walks
//! it. Conditional phases (`when_review`) resolve against `TaskProfile`.

use super::evidence::EvidenceKind;
use super::gate::{Gate, GateKind, GatePredicate};
use super::profile::Risk;
use serde::{Deserialize, Serialize};

/// Phase id within a playbook.
pub type PhaseId = String;

/// One phase: id + whether completing the workflow may skip it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phase {
    pub id: PhaseId,
    /// `false` = `canComplete` fails while this phase is unpassed.
    pub required: bool,
    /// Skip reason template for conditional phases (review on low risk).
    pub skip_when: Option<String>,
}

/// A playbook: ordered phases + the gates each phase needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playbook {
    pub id: String,
    pub phases: Vec<Phase>,
    pub gates: Vec<Gate>,
}

impl Playbook {
    fn phase(id: &str, required: bool, skip_when: Option<&str>) -> Phase {
        Phase {
            id: id.to_string(),
            required,
            skip_when: skip_when.map(|s| s.to_string()),
        }
    }

    fn gate(
        id: &str,
        kind: GateKind,
        required: bool,
        kinds: Vec<EvidenceKind>,
        min_items: usize,
        require_ok: bool,
    ) -> Gate {
        let fresh = kind != GateKind::Reproduction;
        Gate::register(
            id,
            kind,
            required,
            GatePredicate {
                kinds,
                min_items,
                require_ok,
                fresh,
                feature: None,
            },
            false,
        )
        .expect("builtin predicates are pure")
    }

    /// Bug Fix (§56): failing repro + implementation + repro passes + focused
    /// tests; review conditional on risk.
    pub fn bug_fix() -> Self {
        Self {
            id: "bug_fix".to_string(),
            phases: vec![
                Self::phase("reproduce", true, None),
                Self::phase("investigate", true, None),
                Self::phase("implement", true, None),
                Self::phase("verify", true, None),
                Self::phase("review", false, Some("risk == low")),
            ],
            gates: vec![
                // Reading the wrong output reproduces a bug too.
                Self::gate(
                    "reproduce",
                    GateKind::Reproduction,
                    true,
                    vec![
                        EvidenceKind::Command,
                        EvidenceKind::Test,
                        EvidenceKind::File,
                    ],
                    1,
                    false,
                ),
                Self::gate(
                    "verify",
                    GateKind::Test,
                    true,
                    vec![EvidenceKind::Test, EvidenceKind::Command],
                    1,
                    true,
                ),
                Self::gate("review", GateKind::Review, false, vec![], 1, true),
                Self::gate(
                    "live",
                    GateKind::Behavior,
                    false,
                    vec![EvidenceKind::Browser, EvidenceKind::Screenshot],
                    1,
                    true,
                ),
            ],
        }
    }

    /// Investigation: findings only, no implementation gate.
    pub fn investigation() -> Self {
        Self {
            id: "investigation".to_string(),
            phases: vec![
                Self::phase("scope", true, None),
                Self::phase("gather", true, None),
                Self::phase("report", true, None),
            ],
            gates: vec![Self::gate(
                "findings",
                GateKind::Behavior,
                true,
                vec![
                    EvidenceKind::File,
                    EvidenceKind::Command,
                    EvidenceKind::Debugger,
                ],
                1,
                true,
            )],
        }
    }

    /// Feature (§56): build + relevant tests + acceptance criteria; live
    /// app / visual / performance / review conditional.
    pub fn feature() -> Self {
        Self {
            id: "feature".to_string(),
            phases: vec![
                Self::phase("design", true, None),
                Self::phase("implement", true, None),
                Self::phase("verify", true, None),
                Self::phase("review", false, Some("risk == low")),
            ],
            gates: vec![
                Self::gate(
                    "build",
                    GateKind::Build,
                    true,
                    vec![EvidenceKind::Command],
                    1,
                    true,
                ),
                Self::gate(
                    "acceptance",
                    GateKind::Behavior,
                    true,
                    vec![
                        EvidenceKind::Test,
                        EvidenceKind::Browser,
                        EvidenceKind::File,
                    ],
                    1,
                    true,
                ),
                Self::gate("review", GateKind::Review, false, vec![], 1, true),
                Self::gate(
                    "visual",
                    GateKind::Visual,
                    false,
                    vec![EvidenceKind::Screenshot],
                    1,
                    true,
                ),
            ],
        }
    }

    /// Refactor (§56): baseline + implementation + tests + after==before.
    pub fn refactor() -> Self {
        Self {
            id: "refactor".to_string(),
            phases: vec![
                Self::phase("baseline", true, None),
                Self::phase("implement", true, None),
                Self::phase("verify", true, None),
            ],
            gates: vec![
                Self::gate(
                    "baseline",
                    GateKind::Behavior,
                    true,
                    vec![EvidenceKind::Test, EvidenceKind::Command],
                    1,
                    true,
                ),
                Self::gate(
                    "parity",
                    GateKind::Test,
                    true,
                    vec![EvidenceKind::Test, EvidenceKind::Command],
                    1,
                    true,
                ),
            ],
        }
    }

    /// Generic fallback (prototype/performance): one verify gate.
    pub fn generic() -> Self {
        Self {
            id: "generic".to_string(),
            phases: vec![
                Self::phase("work", true, None),
                Self::phase("verify", true, None),
            ],
            gates: vec![Self::gate(
                "verify",
                GateKind::Behavior,
                true,
                vec![],
                1,
                true,
            )],
        }
    }

    /// Look up a V1 playbook by id. Unknown ids are an error — the engine
    /// never invents phases.
    pub fn builtin(id: &str) -> Option<Self> {
        match id {
            "investigation" => Some(Self::investigation()),
            "bug_fix" => Some(Self::bug_fix()),
            "feature" => Some(Self::feature()),
            "refactor" => Some(Self::refactor()),
            "generic" => Some(Self::generic()),
            _ => None,
        }
    }

    /// Conditional phases to skip for this risk (§56 review policy).
    pub fn skipped_for_risk(&self, risk: Risk) -> Vec<&Phase> {
        if risk == Risk::Low {
            self.phases
                .iter()
                .filter(|p| p.skip_when.is_some())
                .collect()
        } else {
            Vec::new()
        }
    }
}

/// All V1 playbook ids (§49).
pub const BUILTINS: &[&str] = &["investigation", "bug_fix", "feature", "refactor", "generic"];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_state::CurrentState;
    use crate::evidence::{Evidence, EvidenceKind, Outcome};
    use crate::gate::GateStatus;

    #[test]
    fn builtins_resolve() {
        for id in BUILTINS {
            assert!(Playbook::builtin(id).is_some(), "{id}");
        }
        assert!(Playbook::builtin("nope").is_none());
    }

    #[test]
    fn low_risk_skips_review() {
        let pb = Playbook::bug_fix();
        let skipped = pb.skipped_for_risk(Risk::Low);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].id, "review");
        assert!(pb.skipped_for_risk(Risk::High).is_empty());
    }

    #[test]
    fn bugfix_has_optional_live_gate_for_browser_evidence() {
        let pb = Playbook::bug_fix();
        let g = pb.gates.iter().find(|g| g.id == "live").unwrap();
        assert!(!g.required);
        let e = Evidence::attributed(
            "s1",
            EvidenceKind::Screenshot,
            &["live"],
            "shot frame F1 seq 1",
            Outcome::Pass,
            "t",
            "c",
        );
        let now = CurrentState::default();
        let e = e.with_code_state(now.bind(&[]));
        let r = g.evaluate(std::slice::from_ref(&e), &now);
        assert_eq!(r.status, GateStatus::Passed);
        assert!(!r.unverified_origin);
    }

    #[test]
    fn feature_has_optional_visual_gate_for_screenshot_evidence() {
        let pb = Playbook::feature();
        let g = pb.gates.iter().find(|g| g.id == "visual").unwrap();
        assert!(!g.required);
        let e = Evidence::attributed(
            "s1",
            EvidenceKind::Screenshot,
            &["visual"],
            "shot frame F1 seq 1",
            Outcome::Pass,
            "t",
            "c",
        );
        let now = CurrentState::default();
        let e = e.with_code_state(now.bind(&[]));
        let r = g.evaluate(std::slice::from_ref(&e), &now);
        assert_eq!(r.status, GateStatus::Passed);
        assert!(!r.unverified_origin);
    }
}

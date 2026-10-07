//! Gate engine (§§52/54): pure predicates over the stored evidence map.
//!
//! A `Gate` declares what evidence it needs; `evaluate` reads items only.
//! Registration REJECTS predicates that need fresh I/O (`GateError::NeedsIo`)
//! — the agent produces evidence first, the gate only reads it.

use super::code_state::CurrentState;
use super::evidence::{Evidence, EvidenceKind, Outcome};
use serde::{Deserialize, Serialize};

/// Max OMP continues per gate before `blocked` + force-escalate (§54).
pub const MAX_CONTINUE: u32 = 3;

/// Stable gate id (also the `for_gates` key evidence references).
pub type GateId = String;

/// Gate kind (§52). `Review` gates read approval evidence; `Custom` carries a
/// free-form predicate id in `predicate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateKind {
    Build,
    Test,
    Lint,
    Reproduction,
    Behavior,
    Visual,
    Performance,
    Review,
    Custom(String),
}

/// Predicate: what stored evidence satisfies this gate. Pure data — the
/// engine matches items, it never fetches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatePredicate {
    /// Evidence kinds accepted (empty = any kind).
    pub kinds: Vec<EvidenceKind>,
    /// Minimum number of ATTRIBUTED supporting items (`ok` as required).
    pub min_items: usize,
    /// When true the supporting items must have `ok: true` (passing result).
    /// `false` for `reproduction` gates: a FAILING repro proves the bug.
    pub require_ok: bool,
    /// Only evidence whose code state still matches counts (ADR-0024).
    /// `false` only for `reproduction` gates: "the bug existed" cannot go
    /// stale (ADR-0036).
    #[serde(default = "fresh_default")]
    pub fresh: bool,
    /// `feature <id> proven` (ADR-0025): only evidence for this
    /// verification-profile feature counts.
    #[serde(default)]
    pub feature: Option<crate::verification::FeatureRef>,
}

fn fresh_default() -> bool {
    true
}

/// A verification gate (§52).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gate {
    pub id: GateId,
    pub kind: GateKind,
    pub required: bool,
    pub predicate: GatePredicate,
}

/// Gate evaluation outcome (§52 results).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    Pending,
    Passed,
    Failed,
    Blocked,
    Skipped,
}

/// Why a gate is not passing: drives the OMP continue / user escalation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateResult {
    pub status: GateStatus,
    /// Ids of evidence items that decided this evaluation.
    pub deciding: Vec<String>,
    /// Machine-readable reason (`missing 2 test evidence for gate "verify"`).
    pub reason: String,
    /// True when an optional gate passed on unattributed evidence only —
    /// the UI must badge it `unverified-origin` (§53 R2 fix).
    pub unverified_origin: bool,
}

/// Registration-time rejection (§54): predicates that cannot be evaluated
/// purely MUST NOT be registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateError {
    /// Predicate needs fresh I/O (run tests, open browser, spawn, RPC).
    /// Produce the evidence via the agent loop first.
    NeedsIo(String),
    /// Only `reproduction` gates may count stale evidence (ADR-0036).
    StaleOnlyForReproduction(String),
}

/// A required gate as OMP's `op=gate` and the `cedian.toml` floor declare it.
/// The one way to build such a gate: `require_ok` and `fresh` follow from
/// the kind, so neither caller can switch them off.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateSpec {
    pub id: GateId,
    pub gate_kind: GateKind,
    #[serde(default)]
    pub evidence_kinds: Vec<EvidenceKind>,
    #[serde(default = "one")]
    pub min_items: usize,
    /// Set by `op=gate` after it checks the profile; never from a file.
    #[serde(skip)]
    pub feature: Option<crate::verification::FeatureRef>,
}

fn one() -> usize {
    1
}

impl GateSpec {
    pub fn build(self) -> Result<Gate, GateError> {
        let repro = self.gate_kind == GateKind::Reproduction;
        Gate::register(
            self.id,
            self.gate_kind,
            true,
            GatePredicate {
                kinds: self.evidence_kinds,
                min_items: self.min_items.max(1),
                require_ok: !repro,
                fresh: !repro,
                feature: self.feature,
            },
            false,
        )
    }
}

impl Gate {
    /// Register a gate. `needs_io: true` rejects with `GateError::NeedsIo`
    /// — the plan forbids scheduling fetches inside the engine.
    pub fn register(
        id: impl Into<String>,
        kind: GateKind,
        required: bool,
        predicate: GatePredicate,
        needs_io: bool,
    ) -> Result<Self, GateError> {
        if needs_io {
            return Err(GateError::NeedsIo(
                "gate predicate requires fresh I/O; produce evidence first".to_string(),
            ));
        }
        let id = id.into();
        if !predicate.fresh && kind != GateKind::Reproduction {
            return Err(GateError::StaleOnlyForReproduction(id));
        }
        Ok(Self {
            id,
            kind,
            required,
            predicate,
        })
    }

    /// Pure evaluation over stored evidence and the current code state
    /// (passed in as data, ADR-0024). `items`: ALL evidence in the workflow
    /// (the engine filters by `for_gates`). An item counts only when:
    /// attributed (required gates), fresh (gates with `predicate.fresh`), and
    /// its outcome fits (`pass` for `require_ok`, `pass|fail` otherwise —
    /// `inconclusive` never counts). A required gate whose ONLY support is
    /// unattributed is `Failed` (provenance rejection), never passed.
    pub fn evaluate(&self, items: &[Evidence], current: &CurrentState) -> GateResult {
        let for_gate: Vec<&Evidence> = items
            .iter()
            .filter(|e| e.for_gates.iter().any(|g| g == &self.id))
            .filter(|e| self.predicate.feature.is_none() || e.feature == self.predicate.feature)
            .collect();
        let supporting: Vec<&Evidence> = for_gate
            .iter()
            .filter(|e| self.predicate.kinds.is_empty() || self.predicate.kinds.contains(&e.kind))
            .copied()
            .collect();
        let wrong_kind = for_gate.len() - supporting.len();
        let (mut unattributed, mut stale, mut inconclusive, mut wrong_outcome) = (0, 0, 0, 0);
        let mut relevant: Vec<&Evidence> = Vec::new();
        for e in &supporting {
            if self.required && !e.is_attributed() {
                unattributed += 1;
            } else if self.predicate.fresh && e.stale_reason(current).is_some() {
                stale += 1;
            } else if e.outcome == Outcome::Inconclusive
                || (self.kind == GateKind::Performance
                    && e.measurement
                        .as_ref()
                        .is_none_or(|m| !m.missing().is_empty()))
            {
                inconclusive += 1;
            } else if self.predicate.require_ok && e.outcome != Outcome::Pass {
                wrong_outcome += 1;
            } else {
                relevant.push(e);
            }
        }
        if relevant.len() >= self.predicate.min_items {
            let unverified = !self.required && relevant.iter().all(|e| !e.is_attributed());
            return GateResult {
                status: GateStatus::Passed,
                deciding: relevant.iter().map(|e| e.id.clone()).collect(),
                reason: format!("gate {:?} satisfied", self.id),
                unverified_origin: unverified,
            };
        }
        // Distinguish provenance rejection from plain absence.
        if self.required && !supporting.is_empty() && unattributed == for_gate.len() {
            return GateResult {
                status: GateStatus::Failed,
                deciding: supporting.iter().map(|e| e.id.clone()).collect(),
                reason: format!(
                    "gate {:?} rejects unattributed evidence (reproduce inside a tracked edit)",
                    self.id
                ),
                unverified_origin: false,
            };
        }
        let rejected: Vec<String> = [
            (stale, "stale"),
            (inconclusive, "inconclusive"),
            (unattributed, "unattributed"),
            (wrong_outcome, "failing"),
            (wrong_kind, "wrong kind"),
        ]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect();
        GateResult {
            status: GateStatus::Pending,
            deciding: supporting.iter().map(|e| e.id.clone()).collect(),
            reason: format!(
                "gate {:?} needs {} attributed{} {} evidence, has {}{}",
                self.id,
                self.predicate.min_items,
                if self.predicate.fresh { " fresh" } else { "" },
                if self.predicate.require_ok {
                    "passing"
                } else {
                    "observed"
                },
                relevant.len(),
                if rejected.is_empty() {
                    String::new()
                } else {
                    format!(" (not counted: {})", rejected.join(", "))
                },
            ),
            unverified_origin: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_state::CodeState;

    fn pred(kinds: Vec<EvidenceKind>, min_items: usize, require_ok: bool) -> GatePredicate {
        GatePredicate {
            kinds,
            min_items,
            require_ok,
            fresh: true,
            feature: None,
        }
    }

    fn gate(id: &str, required: bool, p: GatePredicate) -> Gate {
        Gate::register(id, GateKind::Test, required, p, false).unwrap()
    }

    fn ws(text: &str) -> CurrentState {
        CurrentState::from_files([("a.rs".to_string(), text.as_bytes())])
    }

    /// Attributed item bound to `a.rs` as it is in `seen`.
    fn item(
        id: &str,
        gate: &str,
        kind: EvidenceKind,
        outcome: Outcome,
        seen: &CurrentState,
    ) -> Evidence {
        Evidence::attributed(id, kind, &[gate], "s", outcome, "t", "c")
            .with_code_state(seen.bind(&["a.rs".to_string()]))
    }

    #[test]
    fn needs_io_rejected_at_registration() {
        let r = Gate::register("g", GateKind::Test, true, pred(vec![], 1, true), true);
        assert!(matches!(r, Err(GateError::NeedsIo(_))));
    }

    #[test]
    fn only_reproduction_gates_may_count_stale_evidence() {
        let mut p = pred(vec![], 1, true);
        p.fresh = false;
        let r = Gate::register("g", GateKind::Test, true, p.clone(), false);
        assert!(matches!(r, Err(GateError::StaleOnlyForReproduction(_))));
        assert!(Gate::register("r", GateKind::Reproduction, true, p, false).is_ok());
    }

    #[test]
    fn required_gate_passes_on_attributed_fresh_pass() {
        let now = ws("1");
        let g = gate("verify", true, pred(vec![EvidenceKind::Test], 1, true));
        let r = g.evaluate(
            &[item(
                "e1",
                "verify",
                EvidenceKind::Test,
                Outcome::Pass,
                &now,
            )],
            &now,
        );
        assert_eq!(r.status, GateStatus::Passed);
        assert!(!r.unverified_origin);
    }

    #[test]
    fn required_gate_rejects_unattributed() {
        let g = gate("verify", true, pred(vec![EvidenceKind::Test], 1, true));
        let e = Evidence::unattributed(
            "e1",
            EvidenceKind::Test,
            &["verify"],
            "trust me",
            Outcome::Pass,
        );
        let r = g.evaluate(&[e], &ws("1"));
        assert_eq!(r.status, GateStatus::Failed);
        assert!(r.reason.contains("unattributed"));
    }

    #[test]
    fn stale_evidence_does_not_count_on_a_fresh_gate() {
        let before = ws("1");
        let g = gate("verify", true, pred(vec![EvidenceKind::Test], 1, true));
        let e = item("e1", "verify", EvidenceKind::Test, Outcome::Pass, &before);
        let r = g.evaluate(std::slice::from_ref(&e), &ws("2"));
        assert_eq!(r.status, GateStatus::Pending);
        assert!(r.reason.contains("1 stale"), "{}", r.reason);
        // Unknown code state counts as stale too.
        let mut unknown = e;
        unknown.code_state = None;
        assert_eq!(g.evaluate(&[unknown], &before).status, GateStatus::Pending);
    }

    #[test]
    fn tree_bound_evidence_goes_stale_on_any_edit() {
        let before = ws("1");
        let g = gate("verify", true, pred(vec![], 1, true));
        let e = Evidence::attributed(
            "e1",
            EvidenceKind::Test,
            &["verify"],
            "s",
            Outcome::Pass,
            "t",
            "c",
        )
        .with_code_state(CodeState::Tree(before.tree));
        assert_eq!(
            g.evaluate(std::slice::from_ref(&e), &before).status,
            GateStatus::Passed
        );
        assert_eq!(g.evaluate(&[e], &ws("2")).status, GateStatus::Pending);
    }

    #[test]
    fn repro_gate_counts_stale_failing_observation() {
        let before = ws("buggy");
        let mut p = pred(vec![EvidenceKind::Command], 1, false);
        p.fresh = false;
        let g = Gate::register("repro", GateKind::Reproduction, true, p, false).unwrap();
        let e = item("e1", "repro", EvidenceKind::Command, Outcome::Fail, &before);
        // The fix changed a.rs: the repro is stale and still proves the bug.
        assert_eq!(g.evaluate(&[e], &ws("fixed")).status, GateStatus::Passed);
    }

    #[test]
    fn inconclusive_never_passes() {
        let now = ws("1");
        let test = gate("verify", true, pred(vec![], 1, true));
        let e = item(
            "e1",
            "verify",
            EvidenceKind::Test,
            Outcome::Inconclusive,
            &now,
        );
        let r = test.evaluate(std::slice::from_ref(&e), &now);
        assert_eq!(r.status, GateStatus::Pending);
        assert!(r.reason.contains("1 inconclusive"), "{}", r.reason);
        let repro = gate("verify", true, pred(vec![], 1, false));
        assert_eq!(repro.evaluate(&[e], &now).status, GateStatus::Pending);
    }

    #[test]
    fn failing_evidence_does_not_pass_a_test_gate() {
        let now = ws("1");
        let g = gate("verify", true, pred(vec![], 1, true));
        let r = g.evaluate(
            &[item(
                "e1",
                "verify",
                EvidenceKind::Test,
                Outcome::Fail,
                &now,
            )],
            &now,
        );
        assert_eq!(r.status, GateStatus::Pending);
        assert!(r.reason.contains("1 failing"), "{}", r.reason);
    }

    #[test]
    fn performance_gate_needs_a_complete_measurement() {
        use crate::evidence::Measurement;
        let now = ws("1");
        let g = Gate::register(
            "perf",
            GateKind::Performance,
            true,
            pred(vec![], 1, true),
            false,
        )
        .unwrap();
        let full = Measurement {
            runs: Some(5),
            median: Some(12.0),
            range: Some([11.0, 14.0]),
            limiter: Some("cpu".into()),
            build_profile: Some("release".into()),
        };
        let mut e = item("e1", "perf", EvidenceKind::Command, Outcome::Pass, &now);
        let r = g.evaluate(std::slice::from_ref(&e), &now);
        assert!(
            r.reason.contains("1 inconclusive"),
            "no measurement: {}",
            r.reason
        );
        e.measurement = Some(Measurement {
            build_profile: None,
            ..full.clone()
        });
        assert_eq!(
            g.evaluate(std::slice::from_ref(&e), &now).status,
            GateStatus::Pending
        );
        e.measurement = Some(full);
        assert_eq!(g.evaluate(&[e], &now).status, GateStatus::Passed);
    }

    #[test]
    fn feature_gate_counts_only_that_features_evidence() {
        use crate::verification::FeatureRef;
        let now = ws("1");
        let feature = FeatureRef {
            profile: "verify-notes".into(),
            id: "search".into(),
        };
        let mut p = pred(vec![], 1, true);
        p.feature = Some(feature.clone());
        let g = Gate::register("search", GateKind::Behavior, true, p, false).unwrap();
        let mut e = item("e1", "search", EvidenceKind::Browser, Outcome::Pass, &now);
        assert_eq!(
            g.evaluate(std::slice::from_ref(&e), &now).status,
            GateStatus::Pending
        );
        e.feature = Some(feature);
        assert_eq!(g.evaluate(&[e], &now).status, GateStatus::Passed);
    }

    #[test]
    fn optional_gate_flags_unverified_origin() {
        let now = ws("1");
        let g = gate("live", false, pred(vec![], 1, true));
        let e = Evidence::unattributed(
            "e1",
            EvidenceKind::Browser,
            &["live"],
            "looks fine",
            Outcome::Pass,
        )
        .with_code_state(now.bind(&[]));
        let r = g.evaluate(&[e], &now);
        assert_eq!(r.status, GateStatus::Passed);
        assert!(r.unverified_origin);
    }

    #[test]
    fn wrong_kind_is_named_and_not_called_unattributed() {
        let now = ws("1");
        let g = gate("verify", true, pred(vec![EvidenceKind::Test], 1, true));
        let read = item("e1", "verify", EvidenceKind::File, Outcome::Pass, &now);
        let typed =
            Evidence::unattributed("e2", EvidenceKind::Test, &["verify"], "s", Outcome::Pass);
        let r = g.evaluate(&[read, typed], &now);
        assert_eq!(r.status, GateStatus::Pending);
        assert!(
            r.reason.contains("1 unattributed, 1 wrong kind"),
            "{}",
            r.reason
        );
    }

    #[test]
    fn pending_when_below_min() {
        let now = ws("1");
        let g = gate("verify", true, pred(vec![EvidenceKind::Test], 2, true));
        let r = g.evaluate(
            &[item(
                "e1",
                "verify",
                EvidenceKind::Test,
                Outcome::Pass,
                &now,
            )],
            &now,
        );
        assert_eq!(r.status, GateStatus::Pending);
        assert!(r.reason.contains("has 1"));
    }
}

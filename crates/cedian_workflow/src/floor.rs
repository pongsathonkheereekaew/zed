//! Gate floor (§46, ADR-0010): the gates cedian policy requires per task
//! kind × risk. OMP may ADD gates; it can never remove or weaken a floor
//! gate. An empty floor is the fast lane (ADR-0026): nothing is required
//! unless OMP or the user starts a workflow.
//!
//! Pure data. The policy source is the user's `cedian.toml`
//! (`[[workflow.floor]]`, ADR-0018), parsed as [`FloorRuleSpec`]s so every
//! floor gate goes through [`GateSpec::build`].

use crate::gate::{Gate, GateSpec};
use crate::profile::{Risk, TaskKind, TaskProfile};
use serde::Deserialize;

/// Gates required for one task kind at or above a risk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloorRule {
    pub kind: TaskKind,
    pub min_risk: Risk,
    pub gates: Vec<Gate>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GateFloor {
    pub rules: Vec<FloorRule>,
}

/// One `[[workflow.floor]]` table as written in `cedian.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FloorRuleSpec {
    pub kind: TaskKind,
    pub min_risk: Risk,
    pub gates: Vec<GateSpec>,
}

impl GateFloor {
    /// Build the floor; the error names the rule (1-based) and the gate.
    pub fn from_specs(specs: Vec<FloorRuleSpec>) -> Result<Self, String> {
        let mut rules = Vec::with_capacity(specs.len());
        for (n, spec) in specs.into_iter().enumerate() {
            let gates = spec
                .gates
                .into_iter()
                .map(|g| {
                    let id = g.id.clone();
                    g.build()
                        .map_err(|e| format!("floor rule {}, gate {id:?}: {e:?}", n + 1))
                })
                .collect::<Result<_, _>>()?;
            rules.push(FloorRule {
                kind: spec.kind,
                min_risk: spec.min_risk,
                gates,
            });
        }
        Ok(Self { rules })
    }

    /// Floor gates for this task, all forced `required`.
    pub fn gates_for(&self, task: &TaskProfile) -> Vec<Gate> {
        self.rules
            .iter()
            .filter(|r| r.kind == task.kind && task.risk >= r.min_risk)
            .flat_map(|r| r.gates.iter().cloned())
            .map(|mut g| {
                g.required = true;
                g
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::{GateKind, GatePredicate};

    fn floor() -> GateFloor {
        let gate = Gate::register(
            "build",
            GateKind::Build,
            false, // forced required by the floor
            GatePredicate {
                kinds: vec![],
                min_items: 1,
                require_ok: true,
                fresh: true,
                feature: None,
            },
            false,
        )
        .unwrap();
        GateFloor {
            rules: vec![FloorRule {
                kind: TaskKind::Feature,
                min_risk: Risk::Medium,
                gates: vec![gate],
            }],
        }
    }

    #[test]
    fn floor_applies_by_kind_and_min_risk() {
        let mut task = TaskProfile::new("t", TaskKind::Feature);
        task.risk = Risk::Low;
        assert!(floor().gates_for(&task).is_empty());
        task.risk = Risk::High;
        let gates = floor().gates_for(&task);
        assert_eq!(gates.len(), 1);
        assert!(gates[0].required);
        task.kind = TaskKind::BugFix;
        assert!(floor().gates_for(&task).is_empty());
        assert!(GateFloor::default().gates_for(&task).is_empty());
    }

    fn specs(v: serde_json::Value) -> Result<GateFloor, String> {
        let specs: Vec<FloorRuleSpec> = serde_json::from_value(v).map_err(|e| e.to_string())?;
        GateFloor::from_specs(specs)
    }

    #[test]
    fn specs_build_required_gates_with_kind_rules() {
        let floor = specs(serde_json::json!([{
            "kind": "bug_fix", "min_risk": "low",
            "gates": [{"id": "repro", "gate_kind": "reproduction"},
                      {"id": "tests", "gate_kind": "test", "evidence_kinds": ["test"], "min_items": 0}]
        }]))
        .unwrap();
        let gates = &floor.rules[0].gates;
        assert!(gates.iter().all(|g| g.required));
        assert!(!gates[0].predicate.fresh && !gates[0].predicate.require_ok);
        assert!(gates[1].predicate.fresh && gates[1].predicate.require_ok);
        assert_eq!(gates[1].predicate.min_items, 1);
    }

    #[test]
    fn specs_cannot_carry_engine_fields() {
        for key in ["fresh", "require_ok", "required", "feature"] {
            let err = specs(serde_json::json!([{
                "kind": "feature", "min_risk": "low",
                "gates": [{"id": "g", "gate_kind": "test", key: false}]
            }]))
            .unwrap_err();
            assert!(err.contains("unknown field"), "{key}: {err}");
        }
    }
}

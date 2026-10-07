//! Project verification profile (ADR-0025): a `verify-<app>` OMP skill with
//! five sections (Launch, Doctor, Drive, Evidence, Cleanup) and a feature
//! map. The skill is the user's OMP config: cedian reads it and never writes
//! it (§77). cedian never runs the profile either (§54); it records what
//! OMP reports through `cedian_workflow_update op=profile`, bound to
//! router-log calls like any evidence.
//!
//! Two rules make profile evidence trustworthy (S2 exit):
//! - **Draft.** A profile counts only once one instance ran all five stages
//!   in order (doctor passing), attributed, against the skill text as it is
//!   now. An edited skill is a draft again.
//! - **Instance health.** Evidence from an instance with no passing Doctor
//!   since its last failed or surprising drive (or failed doctor) is
//!   `inconclusive`.

use crate::code_state::content_hash;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The five profile sections, in run order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Launch,
    Doctor,
    Drive,
    Evidence,
    Cleanup,
}

const RUN: [Stage; 5] = [
    Stage::Launch,
    Stage::Doctor,
    Stage::Drive,
    Stage::Evidence,
    Stage::Cleanup,
];

/// A feature-map entry: `feature <id> proven` names one of these.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FeatureRef {
    /// Skill name, e.g. `verify-notes`.
    pub profile: String,
    /// Feature-map id, e.g. `bulk-archive`.
    pub id: String,
}

impl std::fmt::Display for FeatureRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.profile, self.id)
    }
}

/// Feature ids from a profile skill: the `` - `id` `` bullets under its
/// `## Feature map` heading.
pub fn feature_map(skill: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in skill.lines() {
        let line = line.trim();
        if let Some(heading) = line.strip_prefix("## ") {
            inside = heading.trim().eq_ignore_ascii_case("feature map");
            continue;
        }
        if !inside {
            continue;
        }
        let Some(rest) = line.strip_prefix("- `") else {
            continue;
        };
        if let Some((id, _)) = rest.split_once('`') {
            if !id.is_empty() {
                out.push(id.to_string());
            }
        }
    }
    out
}

/// Health of one running instance, as sequence numbers (0 = never).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceHealth {
    pub last_doctor_pass: u64,
    pub last_bad: u64,
    /// How far the current end-to-end run got (index into the five stages).
    pub run_progress: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileRecord {
    /// Hash of the skill text the end-to-end run proved; `None` = draft.
    pub proven_skill: Option<u64>,
    pub instances: BTreeMap<String, InstanceHealth>,
}

/// Every profile's record (`.cedian/verify.json` in the CLI).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileLedger {
    pub profiles: BTreeMap<String, ProfileRecord>,
    pub seq: u64,
}

impl ProfileLedger {
    /// Record one attributed stage report. `skill` is the profile's skill
    /// text now. Returns a line for the agent.
    pub fn record(
        &mut self,
        profile: &str,
        skill: &str,
        instance: &str,
        stage: Stage,
        ok: bool,
        surprising: bool,
    ) -> String {
        self.seq += 1;
        let seq = self.seq;
        let record = self.profiles.entry(profile.to_string()).or_default();
        let health = record.instances.entry(instance.to_string()).or_default();
        let good = ok && !surprising;
        match stage {
            Stage::Doctor if ok => health.last_doctor_pass = seq,
            Stage::Doctor | Stage::Drive if !good => health.last_bad = seq,
            _ => {}
        }
        // End-to-end run: the five stages in order, each good. A fresh
        // launch restarts it; anything else out of order resets it.
        health.run_progress = if stage == Stage::Launch {
            usize::from(good)
        } else if good && RUN.get(health.run_progress) == Some(&stage) {
            health.run_progress + 1
        } else {
            0
        };
        let mut line = format!("{profile} {instance}: {stage:?} recorded");
        if health.run_progress == RUN.len() {
            health.run_progress = 0;
            if record.proven_skill.is_none() {
                line.push_str("; profile proven end to end");
            }
            record.proven_skill = Some(content_hash(skill.as_bytes()));
        }
        if !good {
            line.push_str("; instance needs a passing Doctor before its evidence counts");
        }
        line
    }

    /// `Err(reason)` when evidence from this profile + instance cannot
    /// count: the profile is a draft for this skill text, or the instance
    /// has no passing Doctor since its last bad drive.
    pub fn check(&self, profile: &str, skill: &str, instance: &str) -> Result<(), String> {
        let record = self.profiles.get(profile);
        if record.and_then(|r| r.proven_skill) != Some(content_hash(skill.as_bytes())) {
            return Err(format!(
                "profile {profile} is a draft (never run end to end as written: launch → doctor → drive → evidence → cleanup)"
            ));
        }
        let health = record.and_then(|r| r.instances.get(instance));
        match health {
            Some(h) if h.last_doctor_pass > 0 && h.last_doctor_pass > h.last_bad => Ok(()),
            _ => Err(format!(
                "instance {instance} has no passing Doctor since its last failed or surprising drive"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SKILL: &str = "# verify-notes\n\n## Launch\n…\n\n## Feature map\n\n- `bulk-archive`: archive 3 notes, list shows none\n- `search`: query finds the note\n\n## Cleanup\n- `not-a-feature`\n";

    fn run(ledger: &mut ProfileLedger, instance: &str) {
        for stage in RUN {
            ledger.record("verify-notes", SKILL, instance, stage, true, false);
        }
    }

    #[test]
    fn feature_map_reads_only_its_section() {
        assert_eq!(feature_map(SKILL), ["bulk-archive", "search"]);
        assert!(feature_map("# x\n- `a`\n").is_empty());
    }

    #[test]
    fn draft_until_one_end_to_end_run() {
        let mut l = ProfileLedger::default();
        assert!(
            l.check("verify-notes", SKILL, "i1")
                .unwrap_err()
                .contains("draft")
        );
        // Out of order: not proven.
        l.record("verify-notes", SKILL, "i1", Stage::Launch, true, false);
        l.record("verify-notes", SKILL, "i1", Stage::Drive, true, false);
        l.record("verify-notes", SKILL, "i1", Stage::Doctor, true, false);
        assert!(
            l.check("verify-notes", SKILL, "i1")
                .unwrap_err()
                .contains("draft")
        );
        run(&mut l, "i1");
        assert_eq!(l.check("verify-notes", SKILL, "i1"), Ok(()));
        // Editing the skill makes it a draft again.
        let edited = format!("{SKILL}\n- extra");
        assert!(
            l.check("verify-notes", &edited, "i1")
                .unwrap_err()
                .contains("draft")
        );
    }

    #[test]
    fn failed_doctor_breaks_the_run() {
        let mut l = ProfileLedger::default();
        l.record("verify-notes", SKILL, "i1", Stage::Launch, true, false);
        l.record("verify-notes", SKILL, "i1", Stage::Doctor, false, false);
        for stage in &RUN[2..] {
            l.record("verify-notes", SKILL, "i1", *stage, true, false);
        }
        assert!(
            l.check("verify-notes", SKILL, "i1")
                .unwrap_err()
                .contains("draft")
        );
    }

    #[test]
    fn bad_drive_needs_a_new_passing_doctor() {
        let mut l = ProfileLedger::default();
        run(&mut l, "i1");
        let msg = l.record("verify-notes", SKILL, "i1", Stage::Drive, true, true);
        assert!(msg.contains("needs a passing Doctor"), "{msg}");
        assert!(
            l.check("verify-notes", SKILL, "i1")
                .unwrap_err()
                .contains("no passing Doctor")
        );
        l.record("verify-notes", SKILL, "i1", Stage::Drive, false, false);
        assert!(l.check("verify-notes", SKILL, "i1").is_err());
        l.record("verify-notes", SKILL, "i1", Stage::Doctor, true, false);
        assert_eq!(l.check("verify-notes", SKILL, "i1"), Ok(()));
        // Another instance never had a doctor.
        assert!(
            l.check("verify-notes", SKILL, "i2")
                .unwrap_err()
                .contains("i2")
        );
    }
}

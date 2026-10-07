//! Correction ledger (ADR-0032, S3 exit). cedian appends one row to
//! `.cedian/corrections.jsonl` when a person corrects the agent: a rejected
//! hunk, a reverted turn, a user edit over an agent hunk, a refused
//! completion, an escalation past `max_continue`, a dismissed finding. The
//! agent cannot write rows. OMP proposes classes over them through
//! `cedian_correction_class`; cedian checks each class and calls it
//! `enforced` only with evidence that its enforcer failed on the old code
//! and passes now.

use cedian_workflow::{CurrentState, Evidence, Outcome, Provenance, WorkflowState};
use omp_rpc::HostTool;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub const CORRECTIONS_FILE: &str = ".cedian/corrections.jsonl";
pub const CLASSES_FILE: &str = ".cedian/correction_classes.json";
pub const CORRECTION_CLASS_TOOL: &str = "cedian_correction_class";
pub const CLASSES_SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorrectionKind {
    HunkRejected,
    TurnReverted,
    UserEditedAgentHunk,
    CompletionRefused,
    ContinueEscalated,
    /// Reviewer noise, not implementer error: never grouped with the rest.
    FindingDismissed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Correction {
    pub id: String,
    pub ts: u64,
    pub task: String,
    pub turn: Option<u32>,
    pub kind: CorrectionKind,
    pub path: Option<String>,
    pub hunk_key: Option<String>,
    pub tool_call_id: Option<String>,
    pub model: Option<String>,
    pub excerpt_hash: Option<String>,
}

/// A row before cedian numbers it.
#[derive(Debug, Clone, Default)]
pub struct Event {
    pub turn: Option<u32>,
    pub path: Option<String>,
    pub hunk_key: Option<String>,
    pub tool_call_id: Option<String>,
    pub excerpt: Option<String>,
}

pub fn excerpt_hash(text: &str) -> String {
    format!("{:016x}", cedian_workflow::content_hash(text.as_bytes()))
}

pub fn load(workdir: &Path) -> Result<Vec<Correction>, String> {
    match std::fs::read_to_string(workdir.join(CORRECTIONS_FILE)) {
        Ok(text) => text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).map_err(|e| format!("corrupt corrections.jsonl: {e}")))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("corrections.jsonl: {e}")),
    }
}

/// Append one row. A user edit over an agent hunk is noticed each time the
/// tracker rebuilds, so that kind is recorded once per hunk text.
pub fn record(
    workdir: &Path,
    kind: CorrectionKind,
    event: Event,
) -> Result<Option<String>, String> {
    let rows = load(workdir)?;
    let hash = event.excerpt.as_deref().map(excerpt_hash);
    if kind == CorrectionKind::UserEditedAgentHunk
        && rows.iter().any(|r| {
            r.kind == kind
                && r.path == event.path
                && r.excerpt_hash == hash
                && r.hunk_key == event.hunk_key
        })
    {
        return Ok(None);
    }
    let row = Correction {
        id: format!("c{}", rows.len() + 1),
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        task: crate::session::CLI_TASK.to_string(),
        turn: event.turn,
        kind,
        path: event.path,
        hunk_key: event.hunk_key,
        tool_call_id: event.tool_call_id,
        model: None,
        excerpt_hash: hash,
    };
    std::fs::create_dir_all(workdir.join(".cedian")).map_err(|e| e.to_string())?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(workdir.join(CORRECTIONS_FILE))
        .map_err(|e| format!("corrections.jsonl: {e}"))?;
    let line = serde_json::to_string(&row).map_err(|e| e.to_string())?;
    writeln!(file, "{line}").map_err(|e| format!("corrections.jsonl: {e}"))?;
    Ok(Some(row.id))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Architecture,
    Types,
    Lint,
    Test,
    Docs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorrectionClass {
    pub name: String,
    pub event_ids: Vec<String>,
    pub level: Level,
    pub enforcer: String,
    /// `enforced` (proof checked) or `documented`.
    pub status: String,
    /// Why it is not `enforced`, when it is not.
    pub note: Option<String>,
}

/// Check a proposed class against the ledger, and its proof against the
/// workflow's evidence. Pure: the caller passes rows, evidence and the
/// workspace as it is now.
pub fn check_class(
    args: &Map<String, Value>,
    rows: &[Correction],
    evidence: &dyn Fn(&str) -> Option<Evidence>,
    current: &CurrentState,
) -> Result<CorrectionClass, String> {
    let text = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or("").trim();
    let (name, enforcer) = (text("name"), text("enforcer"));
    if name.is_empty() || enforcer.is_empty() {
        return Err("needs `name` and `enforcer`".to_string());
    }
    let level: Level = serde_json::from_value(json!(text("level")))
        .map_err(|_| "`level` is architecture, types, lint, test or docs".to_string())?;
    let ids: Vec<String> = args
        .get("event_ids")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let mut events = Vec::new();
    for id in &ids {
        events.push(
            rows.iter()
                .find(|r| &r.id == id)
                .ok_or_else(|| format!("no correction {id}"))?,
        );
    }
    let noise = events
        .iter()
        .filter(|e| e.kind == CorrectionKind::FindingDismissed)
        .count();
    if noise != 0 && noise != events.len() {
        return Err(
            "finding_dismissed is reviewer noise: never in one class with implementer corrections"
                .to_string(),
        );
    }
    let turns: BTreeSet<u32> = events.iter().filter_map(|e| e.turn).collect();
    if events.len() < 2 || turns.len() < 2 {
        return Err(format!(
            "a class needs two or more events from two or more turns (got {} event(s), {} turn(s)); a one-off is not a class",
            events.len(),
            turns.len()
        ));
    }
    let (status, note) = match (level, args.get("proof")) {
        (Level::Docs, _) => (
            "documented",
            Some("a docs-level class is never enforced".to_string()),
        ),
        (_, None) => (
            "documented",
            Some("no proof: give `proof` {fail, pass} evidence ids".to_string()),
        ),
        (_, Some(proof)) => match prove(proof, evidence, current) {
            Ok(()) => ("enforced", None),
            Err(why) => ("documented", Some(why)),
        },
    };
    Ok(CorrectionClass {
        name: name.to_string(),
        event_ids: ids,
        level,
        enforcer: enforcer.to_string(),
        status: status.to_string(),
        note,
    })
}

/// ADR-0032 decision 3: the enforcer failed on the old code (attributed,
/// `fail`, no longer fresh) and passes now (attributed, `pass`, fresh).
fn prove(
    proof: &Value,
    evidence: &dyn Fn(&str) -> Option<Evidence>,
    current: &CurrentState,
) -> Result<(), String> {
    let get = |key: &str| -> Result<Evidence, String> {
        let id = proof
            .get(key)
            .and_then(Value::as_str)
            .ok_or(format!("proof needs `{key}`"))?;
        let e = evidence(id).ok_or(format!("no evidence {id}"))?;
        if !matches!(e.provenance, Provenance::Attributed { .. }) {
            return Err(format!("evidence {id} is unattributed"));
        }
        Ok(e)
    };
    let (fail, pass) = (get("fail")?, get("pass")?);
    if fail.outcome != Outcome::Fail {
        return Err(format!("proof fail {} did not fail", fail.id));
    }
    if fail.stale_reason(current).is_none() {
        return Err(format!(
            "proof fail {} ran on the current code, not the mistake",
            fail.id
        ));
    }
    if pass.outcome != Outcome::Pass {
        return Err(format!("proof pass {} did not pass", pass.id));
    }
    if let Some(why) = pass.stale_reason(current) {
        return Err(format!("proof pass {} is stale: {why}", pass.id));
    }
    Ok(())
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ClassStore {
    #[serde(default)]
    snapshot_version: u32,
    classes: Vec<CorrectionClass>,
}

fn save_class(workdir: &Path, class: CorrectionClass) -> Result<(), String> {
    let path = workdir.join(CLASSES_FILE);
    let mut store: ClassStore = match std::fs::read_to_string(&path) {
        Ok(raw) => {
            let s: ClassStore = serde_json::from_str(&raw)
                .map_err(|e| format!("corrupt correction_classes.json: {e}"))?;
            if s.snapshot_version != CLASSES_SNAPSHOT_VERSION {
                return Err(format!(
                    "correction classes too old (got v{}, want v{CLASSES_SNAPSHOT_VERSION})",
                    s.snapshot_version
                ));
            }
            s
        }
        Err(_) => ClassStore::default(),
    };
    store.snapshot_version = CLASSES_SNAPSHOT_VERSION;
    store.classes.retain(|c| c.name != class.name);
    store.classes.push(class);
    let raw = serde_json::to_string_pretty(&store).map_err(|e| e.to_string())?;
    std::fs::write(path, raw).map_err(|e| e.to_string())
}

/// OMP's host tool for proposing a class over recorded corrections.
pub fn correction_class_tool(
    workdir: PathBuf,
    current: impl Fn() -> CurrentState + Send + Sync + 'static,
) -> HostTool {
    let params = json!({
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "event_ids": {"type": "array", "items": {"type": "string"}, "description": "correction ids (c1, c2, ...) from .cedian/corrections.jsonl"},
            "level": {"type": "string", "enum": ["architecture", "types", "lint", "test", "docs"]},
            "enforcer": {"type": "string", "description": "the check that prevents a repeat"},
            "proof": {
                "type": "object",
                "properties": {"fail": {"type": "string"}, "pass": {"type": "string"}},
                "description": "workflow evidence ids: the enforcer failing on the old code, passing now"
            }
        },
        "required": ["name", "event_ids", "level", "enforcer"],
        "additionalProperties": false
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    HostTool::new(
        CORRECTION_CLASS_TOOL,
        "cedian's own host tool (trusted). Propose a class of repeated agent mistakes over the \
         corrections cedian recorded. cedian refuses one-offs and marks the class enforced only \
         with proof that its enforcer fails on the old code and passes now.",
        params,
        move |args, _ctx| {
            let rows = load(&workdir)?;
            let state: Option<WorkflowState> = crate::workflow_store::load(&workdir).ok();
            let lookup = |id: &str| state.as_ref().and_then(|s| s.evidence.get(id).cloned());
            let class = check_class(&args, &rows, &lookup, &current())?;
            let reply = format!(
                "class {} recorded as {}{}",
                class.name,
                class.status,
                class
                    .note
                    .as_deref()
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_default()
            );
            save_class(&workdir, class)?;
            Ok(reply.into())
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, turn: u32, kind: CorrectionKind) -> Correction {
        Correction {
            id: id.into(),
            ts: 0,
            task: "cli".into(),
            turn: Some(turn),
            kind,
            path: Some("/a.rs".into()),
            hunk_key: None,
            tool_call_id: None,
            model: None,
            excerpt_hash: None,
        }
    }

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn rows() -> Vec<Correction> {
        vec![
            row("c1", 1, CorrectionKind::HunkRejected),
            row("c2", 1, CorrectionKind::HunkRejected),
            row("c3", 2, CorrectionKind::TurnReverted),
            row("c4", 3, CorrectionKind::FindingDismissed),
        ]
    }

    fn none(_: &str) -> Option<Evidence> {
        None
    }

    #[test]
    fn a_one_off_is_not_a_class() {
        let cur = CurrentState::default();
        let one_turn = args(
            json!({"name": "x", "event_ids": ["c1", "c2"], "level": "lint", "enforcer": "clippy"}),
        );
        assert!(
            check_class(&one_turn, &rows(), &none, &cur)
                .unwrap_err()
                .contains("two or more turns")
        );
        let unknown = args(
            json!({"name": "x", "event_ids": ["c1", "c9"], "level": "lint", "enforcer": "clippy"}),
        );
        assert!(
            check_class(&unknown, &rows(), &none, &cur)
                .unwrap_err()
                .contains("no correction c9")
        );
        let mixed = args(
            json!({"name": "x", "event_ids": ["c1", "c4"], "level": "lint", "enforcer": "clippy"}),
        );
        assert!(
            check_class(&mixed, &rows(), &none, &cur)
                .unwrap_err()
                .contains("reviewer noise")
        );
    }

    #[test]
    fn two_turns_without_proof_is_documented_and_docs_never_enforced() {
        let cur = CurrentState::default();
        let class = check_class(
            &args(json!({"name": "x", "event_ids": ["c1", "c3"], "level": "lint", "enforcer": "clippy"})),
            &rows(),
            &none,
            &cur,
        )
        .unwrap();
        assert_eq!(class.status, "documented");
        let docs = check_class(
            &args(json!({"name": "y", "event_ids": ["c1", "c3"], "level": "docs", "enforcer": "AGENTS.md",
                          "proof": {"fail": "e1", "pass": "e2"}})),
            &rows(),
            &none,
            &cur,
        )
        .unwrap();
        assert_eq!(docs.status, "documented");
    }

    fn evidence(id: &str, outcome: Outcome, file_text: &str) -> Evidence {
        let state = CurrentState::from_files([("a.rs".to_string(), file_text.as_bytes())]);
        let mut e = Evidence::unattributed(
            id,
            cedian_workflow::EvidenceKind::Test,
            &["lint"],
            "cargo clippy",
            outcome,
        );
        e.provenance = Provenance::Attributed {
            task_id: "cli".into(),
            tool_call_id: format!("call-{id}"),
        };
        e.code_state = Some(state.bind(&[]));
        e
    }

    #[test]
    fn enforced_only_when_the_check_failed_on_the_old_code_and_passes_now() {
        let now = CurrentState::from_files([("a.rs".to_string(), b"fixed".as_slice())]);
        let ev = |id: &str| match id {
            "e1" => Some(evidence("e1", Outcome::Fail, "mistake")),
            "e2" => Some(evidence("e2", Outcome::Pass, "fixed")),
            "e3" => Some(evidence("e3", Outcome::Fail, "fixed")),
            _ => None,
        };
        let class = |fail: &str, pass: &str| {
            check_class(
                &args(json!({"name": "x", "event_ids": ["c1", "c3"], "level": "lint", "enforcer": "clippy",
                              "proof": {"fail": fail, "pass": pass}})),
                &rows(),
                &ev,
                &now,
            )
            .unwrap()
        };
        assert_eq!(class("e1", "e2").status, "enforced");
        let on_head = class("e3", "e2");
        assert_eq!(on_head.status, "documented");
        assert!(on_head.note.unwrap().contains("not the mistake"));
        let swapped = class("e2", "e1");
        assert_eq!(swapped.status, "documented");
    }

    #[test]
    fn user_edit_rows_are_recorded_once_per_hunk_text() {
        let dir = std::env::temp_dir().join(format!("cedian-corr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let edit = || Event {
            turn: Some(1),
            path: Some("/a.rs".into()),
            excerpt: Some("BETA by user".into()),
            ..Event::default()
        };
        assert_eq!(
            record(&dir, CorrectionKind::UserEditedAgentHunk, edit())
                .unwrap()
                .as_deref(),
            Some("c1")
        );
        assert_eq!(
            record(&dir, CorrectionKind::UserEditedAgentHunk, edit()).unwrap(),
            None
        );
        assert_eq!(
            record(&dir, CorrectionKind::HunkRejected, edit())
                .unwrap()
                .as_deref(),
            Some("c2")
        );
        assert_eq!(load(&dir).unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

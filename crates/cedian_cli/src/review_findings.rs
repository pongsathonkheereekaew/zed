//! Reviewer findings (S3 exit, ADR-0039): `.cedian/findings.json` and the
//! `cedian_review_finding` host tool a reviewer reports through. A finding
//! binds to the unresolved hunk it names; one that names no hunk is refused,
//! so the reviewer learns which hunks exist. Other snapshot versions fail
//! closed (ADR-0016).

use cedian_review::{AttachedFinding, FileDiff, FindingSeverity, ReviewFinding, attach};
use omp_rpc::HostTool;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

pub const REVIEW_FINDING_TOOL: &str = "cedian_review_finding";
pub const FINDINGS_SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct FindingStore {
    #[serde(default)]
    snapshot_version: u32,
    pub findings: Vec<AttachedFinding>,
}

fn findings_path(workdir: &Path) -> PathBuf {
    workdir.join(".cedian").join("findings.json")
}

pub fn load(workdir: &Path) -> Result<FindingStore, String> {
    let raw = match std::fs::read_to_string(findings_path(workdir)) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FindingStore::default()),
        Err(e) => return Err(format!("findings.json: {e}")),
    };
    let store: FindingStore =
        serde_json::from_str(&raw).map_err(|e| format!("corrupt findings.json: {e}"))?;
    if store.snapshot_version != FINDINGS_SNAPSHOT_VERSION {
        return Err(format!(
            "findings state too old (got v{}, want v{FINDINGS_SNAPSHOT_VERSION}), \
             re-baseline: run `cedian review reset`",
            store.snapshot_version
        ));
    }
    Ok(store)
}

pub fn save(workdir: &Path, store: &mut FindingStore) -> Result<(), String> {
    store.snapshot_version = FINDINGS_SNAPSHOT_VERSION;
    std::fs::create_dir_all(workdir.join(".cedian")).map_err(|e| e.to_string())?;
    let raw = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    std::fs::write(findings_path(workdir), raw).map_err(|e| e.to_string())
}

/// Parse a reviewer's report and bind it to a hunk of `diff_for(path)`.
/// `line` is 1-based, as a reviewer reads a file.
pub fn record(
    store: &mut FindingStore,
    args: &Map<String, Value>,
    diff_for: impl Fn(&Path) -> Result<FileDiff, String>,
) -> Result<String, String> {
    let text = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or("").trim();
    let path = text("path").trim_start_matches('/');
    let message = text("message");
    let line = args.get("line").and_then(Value::as_u64).unwrap_or(0) as usize;
    if path.is_empty() || message.is_empty() || line == 0 {
        return Err("needs `path`, `line` (1-based) and `message`".to_string());
    }
    let severity = match text("severity") {
        "blocker" => FindingSeverity::Blocker,
        "suggestion" => FindingSeverity::Suggestion,
        "info" => FindingSeverity::Info,
        other => {
            return Err(format!(
                "severity {other:?}: use blocker, suggestion or info"
            ));
        }
    };
    let key = PathBuf::from(format!("/{path}"));
    let diff = diff_for(&key)?;
    let finding = ReviewFinding {
        path: key.display().to_string(),
        start_line: line - 1,
        line_count: args
            .get("count")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize,
        severity,
        message: message.to_string(),
    };
    let id = format!("f{}", store.findings.len() + 1);
    let attached = attach(&id, finding, &diff)?;
    let reply = format!(
        "finding {id} ({}) attached to {} hunk {}",
        text("severity"),
        attached.finding.path,
        attached.hunk
    );
    store.findings.push(attached);
    Ok(reply)
}

/// Each finding with its state against the current review diff, for
/// `cedian review` and the review gate.
pub fn states(workdir: &Path) -> Result<Vec<(AttachedFinding, &'static str)>, String> {
    let store = load(workdir)?;
    if store.findings.is_empty() {
        return Ok(Vec::new());
    }
    let host = cedian_workspace::HostTools::new(workdir);
    let (_review, tracker) = crate::load_tracker(workdir, &host)?;
    Ok(store
        .findings
        .into_iter()
        .map(|f| {
            let state = match tracker.diff(Path::new(&f.finding.path)) {
                _ if f.dismissed.is_some() => "dismissed",
                Ok(diff) if f.blocks(diff) => "open blocker",
                Ok(diff) if f.is_stale(diff) => "stale (its hunk changed)",
                Ok(_) => "open",
                Err(_) => "stale (its hunk changed)",
            };
            (f, state)
        })
        .collect())
}

/// One line per open blocker; any refuses `cedian_complete`. Unreadable
/// findings fail closed.
pub fn open_blockers(workdir: &Path) -> Vec<String> {
    match states(workdir) {
        Ok(all) => all
            .into_iter()
            .filter(|(_, state)| *state == "open blocker")
            .map(|(f, _)| {
                format!(
                    "review: blocker {} on {} hunk {} is open: {} (fix the hunk, or a person runs                      `cedian review dismiss {} <reason>`)",
                    f.id, f.finding.path, f.hunk, f.finding.message, f.id
                )
            })
            .collect(),
        Err(e) => vec![format!("review: findings unreadable, so blockers cannot be checked ({e})")],
    }
}

/// A person closes a finding with a reason; the audit log records it.
pub fn dismiss(workdir: &Path, id: &str, reason: &str) -> Result<String, String> {
    let mut store = load(workdir)?;
    let finding = store
        .findings
        .iter_mut()
        .find(|f| f.id == id)
        .ok_or_else(|| format!("no finding {id}"))?;
    finding.dismiss(reason)?;
    let path = finding.finding.path.clone();
    let excerpt = finding.hunk_text.clone();
    save(workdir, &mut store)?;
    crate::corrections::record(
        workdir,
        crate::corrections::CorrectionKind::FindingDismissed,
        crate::corrections::Event {
            path: Some(path.clone()),
            hunk_key: Some(id.to_string()),
            excerpt: Some(excerpt),
            ..crate::corrections::Event::default()
        },
    )?;
    crate::audit::AuditLog::open(workdir, cedian_omp::Approvals::Cedian(Default::default()))?
        .dismissal(id, reason.trim())?;
    Ok(format!("dismissed {id} on {path}: {}", reason.trim()))
}

/// The reviewer's host tool over `workdir`'s review diff.
pub fn review_finding_tool(workdir: PathBuf) -> HostTool {
    let params = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "workspace-relative file path"},
            "line": {"type": "integer", "description": "1-based line inside a changed hunk"},
            "count": {"type": "integer", "description": "lines covered (default 1)"},
            "severity": {"type": "string", "enum": ["blocker", "suggestion", "info"]},
            "message": {"type": "string"}
        },
        "required": ["path", "line", "severity", "message"],
        "additionalProperties": false
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    HostTool::new(
        REVIEW_FINDING_TOOL,
        "cedian's own host tool (trusted). Report one review finding on a changed hunk. A \
         `blocker` keeps the change from completing until it is fixed or a person dismisses it.",
        params,
        move |args, _ctx| {
            let host = cedian_workspace::HostTools::new(&workdir);
            let (_review, tracker) = crate::load_tracker(&workdir, &host)?;
            let mut store = load(&workdir)?;
            let reply = record(&mut store, &args, |path| {
                tracker
                    .diff(path)
                    .cloned()
                    .map_err(|e| format!("{e}; changed files: {:?}", tracker.paths()))
            })?;
            save(&workdir, &mut store)?;
            Ok(reply.into())
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use cedian_review::{Hunk, HunkStatus};

    fn diff() -> FileDiff {
        FileDiff {
            path: "/notes.txt".to_string(),
            hunks: vec![Hunk {
                before_start: 1,
                before_count: 1,
                after_start: 1,
                after_count: 1,
            }],
            statuses: vec![HunkStatus::Pending],
            snapshot: "alpha\nBETA\ngamma\n".to_string(),
        }
    }

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn report_attaches_with_a_one_based_line() {
        let mut store = FindingStore::default();
        let reply = record(
            &mut store,
            &args(json!({"path": "notes.txt", "line": 2, "severity": "blocker", "message": "BETA should stay lowercase"})),
            |_| Ok(diff()),
        )
        .unwrap();
        assert_eq!(reply, "finding f1 (blocker) attached to /notes.txt hunk 0");
        assert_eq!(store.findings[0].hunk_text, "BETA");
    }

    #[test]
    fn bad_reports_are_refused_with_the_reason() {
        let mut store = FindingStore::default();
        let off = record(
            &mut store,
            &args(json!({"path": "notes.txt", "line": 3, "severity": "blocker", "message": "x"})),
            |_| Ok(diff()),
        )
        .unwrap_err();
        assert!(off.contains("hunk 0: lines 2-2"), "{off}");
        let sev = record(
            &mut store,
            &args(json!({"path": "notes.txt", "line": 2, "severity": "fatal", "message": "x"})),
            |_| Ok(diff()),
        )
        .unwrap_err();
        assert!(sev.contains("use blocker"), "{sev}");
        assert!(store.findings.is_empty());
    }

    #[test]
    fn store_roundtrips_and_other_versions_fail_closed() {
        let dir = std::env::temp_dir().join(format!("cedian-findings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = FindingStore::default();
        record(
            &mut store,
            &args(json!({"path": "notes.txt", "line": 2, "severity": "info", "message": "ok"})),
            |_| Ok(diff()),
        )
        .unwrap();
        save(&dir, &mut store).unwrap();
        assert_eq!(load(&dir).unwrap().findings.len(), 1);
        std::fs::write(dir.join(".cedian/findings.json"), r#"{"findings":[]}"#).unwrap();
        assert!(load(&dir).unwrap_err().contains("cedian review reset"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

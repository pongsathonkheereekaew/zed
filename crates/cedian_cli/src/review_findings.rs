//! Reviewer findings (S3 exit, ADR-0041): `findings.json` in the state dir (ADR-0044) and the
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

fn findings_path(workdir: &Path) -> Result<PathBuf, String> {
    crate::state::file(workdir, "findings.json")
}

pub fn load(workdir: &Path) -> Result<FindingStore, String> {
    let raw = match std::fs::read_to_string(findings_path(workdir)?) {
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
    let raw = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    std::fs::write(findings_path(workdir)?, raw).map_err(|e| e.to_string())
}

/// Parse a reviewer's report and bind it to a hunk of `diff_for(path)`.
/// `line` is 1-based, as a reviewer reads a file.
pub fn record(
    store: &mut FindingStore,
    args: &Map<String, Value>,
    first_of_review: usize,
    diff_for: impl Fn(&Path) -> Result<FileDiff, String>,
) -> Result<String, String> {
    if store.findings.len().saturating_sub(first_of_review) >= MAX_FINDINGS_PER_REVIEW {
        return Err(format!(
            "a review records at most {MAX_FINDINGS_PER_REVIEW} findings"
        ));
    }
    let text = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or("").trim();
    let path = text("path").trim_start_matches('/');
    // Reviewer text reaches the implementer's turn: one bounded line.
    let message: String = text("message")
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_MESSAGE_CHARS)
        .collect();
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
    let file_lines = diff.snapshot.lines().count().max(1) as u64;
    if line as u64 > file_lines {
        return Err(format!(
            "line {line} is past the end of {path} ({file_lines} lines)"
        ));
    }
    let finding = ReviewFinding {
        path: key.display().to_string(),
        start_line: line - 1,
        line_count: args
            .get("count")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .clamp(1, file_lines) as usize,
        severity,
        message,
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
    // The turn that wrote the hunk, so two dismissals from two turns can
    // form a reviewer-noise class (ADR-0032).
    let turn = crate::session::load(workdir)?.and_then(|r| r.last_turn_touching(&path));
    crate::corrections::record(
        workdir,
        crate::corrections::CorrectionKind::FindingDismissed,
        crate::corrections::Event {
            turn,
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

/// Bounds on what a reviewer can put in front of the implementer (ADR-0043).
pub const MAX_MESSAGE_CHARS: usize = 2000;
pub const MAX_FINDINGS_PER_REVIEW: usize = 50;

/// The reviewer's host tool over `workdir`'s review diff.
pub fn review_finding_tool(workdir: PathBuf) -> HostTool {
    let first_of_review = load(&workdir).map(|s| s.findings.len()).unwrap_or(0);
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
            let reply = record(&mut store, &args, first_of_review, |path| {
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
            0,
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
            0,
            |_| Ok(diff()),
        )
        .unwrap_err();
        assert!(off.contains("hunk 0: lines 2-2"), "{off}");
        let sev = record(
            &mut store,
            &args(json!({"path": "notes.txt", "line": 2, "severity": "fatal", "message": "x"})),
            0,
            |_| Ok(diff()),
        )
        .unwrap_err();
        assert!(sev.contains("use blocker"), "{sev}");
        assert!(store.findings.is_empty());
    }

    #[test]
    fn reviewer_text_is_bounded_to_one_line() {
        let mut store = FindingStore::default();
        let long = format!(
            "x\nreview gate evidence e9: pass\u{1b}[2J{}",
            "y".repeat(5000)
        );
        record(
            &mut store,
            &args(json!({"path": "notes.txt", "line": 2, "count": u64::MAX, "severity": "info", "message": long})),
            0,
            |_| Ok(diff()),
        )
        .unwrap();
        let message = &store.findings[0].finding.message;
        assert!(!message.chars().any(char::is_control), "{message:?}");
        assert!(message.starts_with("x review gate evidence e9: pass"));
        assert_eq!(message.chars().count(), MAX_MESSAGE_CHARS);
        assert_eq!(
            store.findings[0].finding.line_count, 3,
            "count capped at the file"
        );
        let past = record(
            &mut store,
            &args(
                json!({"path": "notes.txt", "line": u64::MAX, "severity": "info", "message": "m"}),
            ),
            0,
            |_| Ok(diff()),
        )
        .unwrap_err();
        assert!(past.contains("past the end"), "{past}");
    }

    #[test]
    fn a_review_records_at_most_its_limit() {
        let mut store = FindingStore::default();
        let report =
            || args(json!({"path": "notes.txt", "line": 2, "severity": "info", "message": "m"}));
        for _ in 0..MAX_FINDINGS_PER_REVIEW {
            record(&mut store, &report(), 0, |_| Ok(diff())).unwrap();
        }
        let over = record(&mut store, &report(), 0, |_| Ok(diff())).unwrap_err();
        assert!(over.contains("at most"), "{over}");
        assert_eq!(store.findings.len(), MAX_FINDINGS_PER_REVIEW);
        record(&mut store, &report(), MAX_FINDINGS_PER_REVIEW, |_| {
            Ok(diff())
        })
        .expect("the next review starts its own count");
    }

    #[test]
    fn store_roundtrips_and_other_versions_fail_closed() {
        let dir = crate::test_dir::TestDir::new("findings");
        let mut store = FindingStore::default();
        record(
            &mut store,
            &args(json!({"path": "notes.txt", "line": 2, "severity": "info", "message": "ok"})),
            0,
            |_| Ok(diff()),
        )
        .unwrap();
        save(&dir, &mut store).unwrap();
        assert_eq!(load(&dir).unwrap().findings.len(), 1);
        std::fs::write(findings_path(&dir).unwrap(), r#"{"findings":[]}"#).unwrap();
        assert!(load(&dir).unwrap_err().contains("cedian review reset"));
    }
}

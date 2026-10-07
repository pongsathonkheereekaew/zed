//! `cedian_review_request` (S3 exit, ADR-0039): the implementing OMP turn
//! asks for a review, and cedian runs the reviewer as its own OMP process:
//! the reviewer spawn profile, under `sandbox-exec` with a generated profile,
//! on the `[review] model`, in a fresh session, with `cedian_review_finding`
//! as its only host tool. The reviewer's tool calls and the dialogs headless
//! refuses for it go to the same audit log.

use crate::review_findings::{self, REVIEW_FINDING_TOOL};
use cedian_omp::sandbox::{REVIEWER_PROFILE_FILE, ReviewerSandbox, resolve_allow_list};
use cedian_omp::{
    Approvals, BashRule, OmpBinary, OmpRuntime, RuntimeConfig, SpawnPolicy, ToolPolicy,
};
use cedian_review::{FileDiff, HunkStatus};
use omp_rpc::HostTool;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const REVIEW_REQUEST_TOOL: &str = "cedian_review_request";

/// The reviewer's session directory: fixed per implementer session, so a
/// replay can install the reviewer's recorded turn there.
pub fn reviewer_dir(session_dir: &Path, workdir: &Path) -> Result<PathBuf, String> {
    let dir = session_dir.join("reviewer");
    if dir.starts_with(workdir) {
        return Err(format!(
            "the reviewer session dir {} is inside the workspace, which the reviewer \
             sandbox cannot write; set CEDIAN_SESSION_DIR outside it",
            dir.display()
        ));
    }
    Ok(dir)
}

/// The open hunks as the reviewer reads them: path, line range, and the
/// before and after lines.
pub fn render_diff(diffs: &[FileDiff], baseline: &HashMap<PathBuf, String>) -> String {
    let mut out = String::new();
    for diff in diffs {
        let before: Vec<&str> = baseline
            .get(Path::new(&diff.path))
            .map(|t| t.lines().collect())
            .unwrap_or_default();
        let after: Vec<&str> = diff.snapshot.lines().collect();
        for (i, h) in diff.hunks.iter().enumerate() {
            if matches!(
                diff.statuses[i],
                HunkStatus::Accepted | HunkStatus::Rejected
            ) {
                continue;
            }
            let path = diff.path.trim_start_matches('/');
            out.push_str(&format!(
                "{path} hunk {i}: lines {}-{}\n",
                h.after_start + 1,
                h.after_start + h.after_count.max(1)
            ));
            for line in before.iter().skip(h.before_start).take(h.before_count) {
                out.push_str(&format!("-{line}\n"));
            }
            for line in after.iter().skip(h.after_start).take(h.after_count) {
                out.push_str(&format!("+{line}\n"));
            }
        }
    }
    out
}

/// Each allow-listed command becomes an `allow` pattern; anything else
/// `bash` runs falls to `prompt`, which headless refuses.
pub fn allow_patterns(allow_list: &[String]) -> Vec<BashRule> {
    allow_list
        .iter()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .map(|c| BashRule {
            pattern: format!("{c}*"),
            approval: ToolPolicy::Allow,
        })
        .collect()
}

fn prompt(diff: &str, focus: &str) -> String {
    let focus = if focus.trim().is_empty() {
        String::new()
    } else {
        format!(" Focus: {}.", focus.trim())
    };
    format!(
        "You are a code reviewer working for the cedian IDE. You cannot edit files. Review this \
         change for correctness bugs and regressions.{focus} Read files if you need context. For \
         each problem, call the `{REVIEW_FINDING_TOOL}` tool with `path`, `line` (1-based, inside \
         a changed hunk), `severity` and `message`. Use `blocker` only for a defect that must be \
         fixed before the change is done; otherwise `suggestion` or `info`. Report nothing you \
         did not check. When done, reply with one line: review-done <number of findings>.\n\n\
         The change:\n{diff}"
    )
}

/// Run one review. Returns the summary the implementing turn sees.
pub fn run_review(
    workdir: &Path,
    session_dir: &Path,
    settings: &cedian_shell::Settings,
    focus: &str,
    live: Option<&cedian_workspace::HostTools>,
) -> Result<String, String> {
    if let Some(live) = live {
        crate::flush_turn_for_review(workdir, live)?;
    }
    let host = cedian_workspace::HostTools::new(workdir);
    let (store, tracker) = crate::load_tracker(workdir, &host)?;
    let diffs: Vec<FileDiff> = tracker
        .paths()
        .iter()
        .filter_map(|p| tracker.diff(p).ok().cloned())
        .collect();
    let (_, baseline) = store.tracker_inputs();
    let diff = render_diff(&diffs, &baseline);
    if diff.is_empty() {
        return Err("nothing to review: no open hunks in this task".to_string());
    }

    let dir = reviewer_dir(session_dir, workdir)?;
    let _ = std::fs::remove_dir_all(dir.join("sessions"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("reviewer dir: {e}"))?;
    let binary =
        std::fs::canonicalize(crate::omp_binary_path()?).map_err(|e| format!("omp binary: {e}"))?;
    let (exec_allow, mut notes) = resolve_allow_list(
        &settings.reviewer_allow_list,
        std::env::var("PATH").ok().as_deref(),
    );
    let home = PathBuf::from(std::env::var("HOME").map_err(|_| "HOME is not set")?);
    let profile = ReviewerSandbox {
        omp_binary: binary.clone(),
        workspace: workdir.to_path_buf(),
        session_dir: dir.clone(),
        omp_run_dir: home.join(".omp/run/daemons"),
        exec_allow,
    }
    .profile()?;
    let profile_path = dir.join(REVIEWER_PROFILE_FILE);
    std::fs::write(&profile_path, profile).map_err(|e| format!("reviewer profile: {e}"))?;
    if settings.review_model.is_none() {
        notes.push(
            "the reviewer runs on OMP's default model, likely the implementer's; set \
             [review] model in cedian.toml (ADR-0011)"
                .to_string(),
        );
    }

    let mut policy = SpawnPolicy {
        approvals: Approvals::Reviewer,
        bash_patterns: allow_patterns(&settings.reviewer_allow_list),
        config_allows: crate::omp_config_allows(workdir)?,
        model: settings.review_model.clone(),
        sandbox_profile: Some(profile_path),
        ..SpawnPolicy::default()
    };
    policy.host_tools.insert(REVIEW_FINDING_TOOL.to_string());
    let mut rt = OmpRuntime::spawn(RuntimeConfig {
        binary: OmpBinary::Bundled(binary),
        session_dir: dir,
        cwd: workdir.to_path_buf(),
        ask_dialog: true,
        prompt_timeout: Duration::from_secs(600),
        policy,
    })
    .map_err(|e| format!("reviewer spawn: {e}"))?;
    rt.deny_ui_requests();
    rt.set_host_tools(vec![review_findings::review_finding_tool(
        workdir.to_path_buf(),
    )])
    .map_err(|e| e.to_string())?;
    let router = rt.router();
    let (sub, events) = router.subscribe();
    let before = review_findings::load(workdir)?.findings.len();
    let turn = rt.prompt(&prompt(&diff, focus), vec![]);

    let mut audit = crate::audit::AuditLog::open(workdir, Approvals::Reviewer)?;
    for event in events.try_iter() {
        audit.record(&event)?;
    }
    router.unsubscribe(sub);
    for refusal in rt.take_refused_ui_requests() {
        audit.refusal(&refusal)?;
        notes.push(format!("refused for the reviewer: {}", refusal.label));
    }
    let _ = rt.shutdown();
    turn.map_err(|e| format!("reviewer turn: {e}"))?;

    let found = review_findings::load(workdir)?.findings.split_off(before);
    let mut reply = format!(
        "reviewer ({}) reported {} finding(s)",
        settings
            .review_model
            .as_deref()
            .unwrap_or("OMP default model"),
        found.len()
    );
    for f in &found {
        reply.push_str(&format!(
            "\n- {} {:?} {} hunk {}: {}",
            f.id, f.finding.severity, f.finding.path, f.hunk, f.finding.message
        ));
    }
    if found
        .iter()
        .any(|f| f.finding.severity == cedian_review::FindingSeverity::Blocker)
    {
        reply.push_str("\nA blocker keeps the review gate unmet until its hunk is fixed or a person dismisses it.");
    }
    for note in notes {
        reply.push_str(&format!("\nnote: {note}"));
    }
    Ok(reply)
}

/// The implementer's host tool.
pub fn review_request_tool(
    workdir: PathBuf,
    session_dir: PathBuf,
    settings: cedian_shell::Settings,
    live: std::sync::Arc<cedian_workspace::HostTools>,
) -> HostTool {
    let params = json!({
        "type": "object",
        "properties": {
            "focus": {"type": "string", "description": "what to look at hardest (optional)"}
        },
        "additionalProperties": false
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    HostTool::new(
        REVIEW_REQUEST_TOOL,
        "cedian's own host tool (trusted). Ask the cedian IDE for an independent review of this \
         task's changes. A separate read-only reviewer runs and reports findings on your hunks; \
         blockers must be fixed before the work is done. Returns the findings.",
        params,
        move |args, _ctx| {
            let focus = args.get("focus").and_then(Value::as_str).unwrap_or("");
            run_review(&workdir, &session_dir, &settings, focus, Some(&live))
                .map(Into::into)
                .map_err(Into::into)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use cedian_review::Hunk;

    #[test]
    fn diff_lists_open_hunks_with_before_and_after_lines() {
        let diff = FileDiff {
            path: "/notes.txt".to_string(),
            hunks: vec![
                Hunk {
                    before_start: 1,
                    before_count: 1,
                    after_start: 1,
                    after_count: 1,
                },
                Hunk {
                    before_start: 2,
                    before_count: 1,
                    after_start: 2,
                    after_count: 1,
                },
            ],
            statuses: vec![HunkStatus::Pending, HunkStatus::Accepted],
            snapshot: "alpha\nBETA\nGAMMA\n".to_string(),
        };
        let baseline = HashMap::from([(
            PathBuf::from("/notes.txt"),
            "alpha\nbeta\ngamma\n".to_string(),
        )]);
        assert_eq!(
            render_diff(&[diff], &baseline),
            "notes.txt hunk 0: lines 2-2\n-beta\n+BETA\n",
            "accepted hunks are not reviewed"
        );
    }

    #[test]
    fn allow_list_becomes_allow_patterns() {
        let rules = allow_patterns(&["git diff".to_string(), " ".to_string(), "ls".to_string()]);
        let patterns: Vec<&str> = rules.iter().map(|r| r.pattern.as_str()).collect();
        assert_eq!(patterns, ["git diff*", "ls*"]);
        assert!(rules.iter().all(|r| r.approval == ToolPolicy::Allow));
    }

    #[test]
    fn reviewer_session_must_sit_outside_the_workspace() {
        let ws = Path::new("/private/tmp/ws");
        assert!(reviewer_dir(Path::new("/private/tmp/ws/.cedian/s"), ws).is_err());
        assert_eq!(
            reviewer_dir(Path::new("/private/tmp/sessions"), ws).unwrap(),
            Path::new("/private/tmp/sessions/reviewer")
        );
    }
}

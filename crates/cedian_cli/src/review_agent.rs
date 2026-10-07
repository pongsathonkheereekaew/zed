//! `cedian_review_request` (S3 exit, ADR-0041): the implementing OMP turn
//! asks for a review, and cedian runs the reviewer as its own OMP process:
//! the reviewer spawn profile, under `sandbox-exec` with a generated profile,
//! on the `[review] model`, in a fresh session, with `cedian_review_finding`
//! as its only host tool. The reviewer's tool calls and the dialogs headless
//! refuses for it go to the same audit log.

use crate::review_findings::{self, REVIEW_FINDING_TOOL};
use cedian_omp::sandbox::{ReviewerLayout, ReviewerSandbox, credential_paths, resolve_allow_list};
use cedian_omp::{
    Approvals, BashRule, OmpBinary, OmpRuntime, RuntimeConfig, SpawnPolicy, ToolPolicy,
};
use cedian_review::{FileDiff, HunkStatus};
use cedian_workflow::Outcome;
use omp_rpc::HostTool;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const REVIEW_REQUEST_TOOL: &str = "cedian_review_request";

/// The reviewer's directory: fixed per implementer session, so a replay can
/// install the reviewer's recorded turn there. Canonical, because Seatbelt
/// matches resolved paths and a symlink must not hide the workspace.
pub fn reviewer_dir(session_dir: &Path, workdir: &Path) -> Result<PathBuf, String> {
    let dir = session_dir.join("reviewer");
    std::fs::create_dir_all(&dir).map_err(|e| format!("reviewer dir: {e}"))?;
    let dir = std::fs::canonicalize(&dir).map_err(|e| format!("reviewer dir: {e}"))?;
    let workdir = std::fs::canonicalize(workdir).map_err(|e| format!("workspace: {e}"))?;
    if dir.starts_with(&workdir) {
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

/// Each allow-listed command `c` becomes the patterns `c` and `c *`, so
/// `ls` never admits `lsof` (ADR-0043); anything else `bash` runs falls to
/// `prompt`, which headless refuses.
pub fn allow_patterns(allow_list: &[String]) -> Vec<BashRule> {
    allow_list
        .iter()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .flat_map(|c| [c.to_string(), format!("{c} *")])
        .map(|pattern| BashRule {
            pattern,
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

/// The model a role maps to in OMP's `modelRoles` record: the role, else
/// `default` (with a note), else none (OMP's own routing, with a note).
pub fn role_model(record: &Value, role: &str) -> (Option<String>, Option<String>) {
    let get = |key: &str| record.get(key).and_then(Value::as_str).map(str::to_string);
    match (get(role), get("default")) {
        (Some(model), _) => (Some(model), None),
        (None, Some(model)) => (
            Some(model),
            Some(format!(
                "no `{role}` model role in OMP's modelRoles: the reviewer runs on `default`"
            )),
        ),
        (None, None) => (
            None,
            Some("OMP's modelRoles names no model: the reviewer runs on OMP's routing".to_string()),
        ),
    }
}

/// ADR-0039 decision 4: independent only when every model that answered
/// in the review differs from every model that edited the task, and both
/// sides are known.
pub fn independent(reviewer: &[String], editors: &BTreeSet<String>) -> bool {
    !reviewer.is_empty() && !editors.is_empty() && reviewer.iter().all(|m| !editors.contains(m))
}

/// What the review shows the `review` gate: a same-model review is never a
/// pass; an independent one fails on a new blocker.
pub fn review_outcome(independent: bool, blocker: bool) -> Outcome {
    match (independent, blocker) {
        (false, _) => Outcome::Inconclusive,
        (true, true) => Outcome::Fail,
        (true, false) => Outcome::Pass,
    }
}

/// Who asked for the review. Inside an implementer turn: its live buffers,
/// the models that answered so far, the asking call and the workflow
/// channel. From the CLI: the stored task only.
#[derive(Default)]
pub struct Requester<'a> {
    pub live: Option<&'a cedian_workspace::HostTools>,
    pub implementer_models: Vec<String>,
    pub tool_call_id: Option<String>,
    pub channel: Option<&'a cedian_workflow::WorkflowChannel>,
}

/// Run one review. Returns the summary the implementing turn sees.
pub fn run_review(
    workdir: &Path,
    session_dir: &Path,
    settings: &cedian_shell::Settings,
    focus: &str,
    requester: Requester,
) -> Result<String, String> {
    if let Some(live) = requester.live {
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
    let mut editors = store.models.clone();
    editors.extend(requester.implementer_models.iter().cloned());

    let layout = ReviewerLayout {
        dir: reviewer_dir(session_dir, workdir)?,
    };
    layout
        .reset()
        .map_err(|e| format!("reviewer run dir: {e}"))?;
    let binary =
        std::fs::canonicalize(crate::omp_binary_path()?).map_err(|e| format!("omp binary: {e}"))?;
    let (exec_allow, mut notes) = resolve_allow_list(
        &settings.reviewer_allow_list,
        std::env::var("PATH").ok().as_deref(),
    );
    // ADR-0039 decision 3: read the role from a directory cedian owns, so a
    // workspace's .omp/config.yml cannot choose who reviews it. The reviewer
    // cannot write it either: its profile allows only its run dir.
    let roles_dir = session_dir.join("roles");
    std::fs::create_dir_all(&roles_dir).map_err(|e| format!("roles dir: {e}"))?;
    let role = &settings.review_role.0;
    let model = match cedian_omp::omp_config_get(
        &binary,
        &roles_dir,
        "modelRoles",
        Duration::from_secs(10),
    ) {
        Ok(record) => {
            let (model, note) = role_model(&record, role);
            notes.extend(note);
            model
        }
        Err(e) => {
            notes.push(format!(
                "cannot read OMP's modelRoles ({e}): the reviewer runs on OMP's routing"
            ));
            None
        }
    };
    let home = PathBuf::from(std::env::var("HOME").map_err(|_| "HOME is not set")?);
    let profile = ReviewerSandbox {
        omp_binary: binary.clone(),
        workspace: workdir.to_path_buf(),
        run_dir: layout.run(),
        read_deny: credential_paths(&home),
        exec_allow,
    }
    .profile()?;
    std::fs::write(layout.profile(), profile).map_err(|e| format!("reviewer profile: {e}"))?;
    // Opened before the reviewer runs: a log that cannot be written stops
    // the review instead of leaving its tool calls unrecorded.
    let mut audit = crate::audit::AuditLog::open(workdir, Approvals::Reviewer)?;

    let mut policy = SpawnPolicy {
        approvals: Approvals::Reviewer,
        bash_patterns: allow_patterns(&settings.reviewer_allow_list),
        config_allows: crate::omp_config_allows(workdir)?,
        model,
        sandbox: Some(layout.clone()),
        ..SpawnPolicy::default()
    };
    policy.host_tools.insert(REVIEW_FINDING_TOOL.to_string());
    let mut rt = OmpRuntime::spawn(RuntimeConfig {
        binary: OmpBinary::Bundled(binary),
        session_dir: layout.session(),
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

    let mut recorded = Ok(());
    for event in events.try_iter() {
        recorded = recorded.and_then(|()| audit.record(&event));
    }
    router.unsubscribe(sub);
    for refusal in rt.take_refused_ui_requests() {
        recorded = recorded.and_then(|()| audit.refusal(&refusal));
        notes.push(format!("refused for the reviewer: {}", refusal.label));
    }
    let reviewer_models = router.answered_models();
    // Shut down before any `?`: an error must not leave the reviewer running.
    let _ = rt.shutdown();
    recorded?;
    turn.map_err(|e| format!("reviewer turn: {e}"))?;

    let independent = independent(&reviewer_models, &editors);
    let editors: Vec<String> = editors.into_iter().collect();
    let mut findings = review_findings::load(workdir)?;
    for f in findings.findings.iter_mut().skip(before) {
        f.reviewer_model = Some(reviewer_models.join(", "));
        f.implementer_models = editors.clone();
    }
    review_findings::save(workdir, &mut findings)?;
    let found = findings.findings.split_off(before);
    let blocker = found
        .iter()
        .any(|f| f.finding.severity == cedian_review::FindingSeverity::Blocker);
    audit.review(role, &reviewer_models, &editors, independent)?;

    let models = format!(
        "reviewer {}, implementer {}",
        or_unknown(&reviewer_models),
        or_unknown(&editors)
    );
    let mut reply = format!(
        "reviewer (role {role}: {}) reported {} finding(s)",
        or_unknown(&reviewer_models),
        found.len()
    );
    // The reviewer's words are quoted data for the implementer, never
    // instructions (ADR-0043): each message is one line, bounded at record.
    for f in &found {
        reply.push_str(&format!(
            "\n- {} {:?} {} hunk {}, reviewer wrote: {:?}",
            f.id, f.finding.severity, f.finding.path, f.hunk, f.finding.message
        ));
    }
    if blocker {
        reply.push_str("\nA blocker keeps the review gate unmet until its hunk is fixed or a person dismisses it.");
    }
    reply.push_str(&if independent {
        format!("\nindependent review ({models})")
    } else {
        format!("\nNOT an independent review ({models}): it counts as inconclusive (ADR-0039)")
    });
    if let Some(channel) = requester.channel {
        let outcome = review_outcome(independent, blocker);
        let summary = format!("review: {} finding(s), {models}", found.len());
        if let Some(id) = channel.cedian_evidence(
            "review",
            outcome,
            &summary,
            requester.tool_call_id.as_deref(),
        )? {
            reply.push_str(&format!(
                "\nreview gate evidence {id}: {}",
                outcome_word(outcome)
            ));
        }
    }
    for note in notes {
        reply.push_str(&format!("\nnote: {note}"));
    }
    Ok(reply)
}

fn or_unknown(models: &[String]) -> String {
    if models.is_empty() {
        "unknown".to_string()
    } else {
        models.join(", ")
    }
}

fn outcome_word(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Pass => "pass",
        Outcome::Fail => "fail",
        Outcome::Inconclusive => "inconclusive",
    }
}

/// The implementer's host tool.
pub fn review_request_tool(
    workdir: PathBuf,
    session_dir: PathBuf,
    settings: cedian_shell::Settings,
    live: std::sync::Arc<cedian_workspace::HostTools>,
    implementer: std::sync::Arc<cedian_omp::EventRouter>,
    channel: std::sync::Arc<cedian_workflow::WorkflowChannel>,
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
        move |args, ctx| {
            let focus = args.get("focus").and_then(Value::as_str).unwrap_or("");
            let requester = Requester {
                live: Some(&live),
                implementer_models: implementer.answered_models(),
                tool_call_id: Some(ctx.tool_call_id().to_string()),
                channel: Some(&channel),
            };
            run_review(&workdir, &session_dir, &settings, focus, requester)
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
    fn the_role_picks_the_model_and_falls_back_to_default() {
        let roles = json!({"default": "opencode-go/muse", "review": "opencode-go/glm-5.3"});
        assert_eq!(
            role_model(&roles, "review"),
            (Some("opencode-go/glm-5.3".into()), None)
        );
        let (model, note) = role_model(&roles, "review-alt");
        assert_eq!(model.as_deref(), Some("opencode-go/muse"));
        assert!(note.unwrap().contains("runs on `default`"));
        assert_eq!(role_model(&json!({}), "review").0, None);
    }

    #[test]
    fn only_a_review_by_another_model_is_independent_and_can_pass() {
        let editors: BTreeSet<String> = ["opencode-go/muse".to_string()].into();
        assert!(independent(&["opencode-go/glm".into()], &editors));
        assert!(
            !independent(&["opencode-go/muse".into()], &editors),
            "same model"
        );
        assert!(!independent(
            &["opencode-go/glm".into(), "opencode-go/muse".into()],
            &editors
        ));
        assert!(!independent(&[], &editors), "reviewer unknown");
        assert!(
            !independent(&["opencode-go/glm".into()], &BTreeSet::new()),
            "editors unknown"
        );
        assert_eq!(review_outcome(false, false), Outcome::Inconclusive);
        assert_eq!(review_outcome(true, true), Outcome::Fail);
        assert_eq!(review_outcome(true, false), Outcome::Pass);
    }

    #[test]
    fn allow_list_becomes_exact_allow_patterns() {
        let rules = allow_patterns(&["git diff".to_string(), " ".to_string(), "ls".to_string()]);
        let patterns: Vec<&str> = rules.iter().map(|r| r.pattern.as_str()).collect();
        assert_eq!(patterns, ["git diff", "git diff *", "ls", "ls *"]);
        assert!(rules.iter().all(|r| r.approval == ToolPolicy::Allow));
    }

    #[test]
    fn reviewer_dir_must_resolve_outside_the_workspace() {
        let root = std::env::temp_dir().join(format!("cedian-revdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ws = root.join("ws");
        std::fs::create_dir_all(ws.join(".state")).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        assert!(reviewer_dir(&ws.join(".state"), &ws).is_err());
        // A path outside the workspace that resolves inside it.
        std::os::unix::fs::symlink(ws.join(".state"), root.join("link")).unwrap();
        assert!(reviewer_dir(&root.join("link"), &ws).is_err());
        assert_eq!(
            reviewer_dir(&root.join("sessions"), &ws).unwrap(),
            root.join("sessions/reviewer")
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

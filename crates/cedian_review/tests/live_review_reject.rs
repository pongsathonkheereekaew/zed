//! Phase 5 live smoke: agent edit → review → reject restores baseline.
//!
//! Plan acceptance (headless): task baseline + agent edit via host tool +
//! diff shows exactly the agent hunk + reject applies the inverse patch.
//!
//! Requires ambient OMP auth. Ignored by default:
//! `cargo test -p cedian_review -- --ignored --nocapture live_review_reject`

use cedian_omp::{OmpBinary, OmpRuntime, RuntimeConfig};
use cedian_review::{AgentEdit, Baseline, HunkStatus, ProvenanceStore, ReviewTracker};
use cedian_workspace::{HostTools, Version, WorkspaceHost};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn dev_config(tag: &str) -> RuntimeConfig {
    RuntimeConfig {
        binary: OmpBinary::Path("omp".to_string()),
        session_dir: std::env::temp_dir()
            .join(format!("cedian-phase5-{tag}-{}", std::process::id())),
        cwd: std::env::temp_dir(),
        ask_dialog: true,
        prompt_timeout: Duration::from_secs(240),
        policy: cedian_omp::SpawnPolicy {
            host_tools: [cedian_workspace::APPLY_EDIT_TOOL.to_string()].into(),
            ..Default::default()
        },
    }
}

#[test]
#[ignore]
fn live_review_reject() {
    let host = HostTools::shared(Path::new("/"));
    let path = Path::new("/review.txt");
    host.open(path, "alpha\nbeta\ngamma\n");

    // Task start: snapshot baseline (versions + texts).
    let mut baseline = Baseline::new();
    baseline.snapshot(path, host.buffer_version(path).unwrap());
    let mut texts = HashMap::new();
    texts.insert(path.to_path_buf(), host.read_buffer(path).unwrap());

    let mut rt = OmpRuntime::spawn(dev_config("review")).expect("spawn");
    rt.set_host_tools(vec![host.apply_edit_tool()])
        .expect("host tools");
    let _ = &host;

    // Agent edits line 2 through the host tool (beta → BETA).
    let turn = rt
        .prompt(
            "Call cedian_apply_edit with path '/review.txt', expected_version 0, start 6, end 10, \
             replacement 'BETA'. Then reply with only: edited-ok",
            vec![],
        )
        .expect("prompt");
    assert_eq!(turn.assistant_text.as_deref(), Some("edited-ok"));
    assert_eq!(
        host.read_buffer(path).as_deref(),
        Some("alpha\nBETA\ngamma\n")
    );

    // Provenance snapshot at tool_execution_end.
    let mut provenance = ProvenanceStore::new();
    provenance.record(AgentEdit {
        tool_call_id: "live-c1".to_string(),
        task_id: "task-1".to_string(),
        file: "/review.txt".to_string(),
        before: "alpha\nbeta\ngamma\n".to_string(),
        after: host.read_buffer(path).unwrap(),
        timestamp_ms: 1,
    });
    assert_eq!(
        provenance.lookup("task-1", "live-c1").unwrap().after,
        "alpha\nBETA\ngamma\n"
    );

    // Review: exactly one hunk (the agent's line).
    let mut tracker = ReviewTracker::new("task-1", baseline, texts);
    let edits: Vec<AgentEdit> = provenance
        .edits_for_task("task-1")
        .into_iter()
        .cloned()
        .collect();
    tracker.attribute(&edits);
    let rebuilt = tracker.rebuild_now(host.as_ref());
    assert_eq!(rebuilt, vec![PathBuf::from("/review.txt")]);
    let diff = tracker.diff(path).unwrap();
    assert_eq!(diff.len(), 1, "exactly the agent hunk");
    assert_eq!(diff.statuses[0], HunkStatus::Pending);

    // Reject → inverse patch → baseline text back, version bumped.
    let v_before: Version = host.buffer_version(path).unwrap();
    tracker.reject_hunk(path, 0, host.as_ref()).unwrap();
    assert_eq!(
        host.read_buffer(path).as_deref(),
        Some("alpha\nbeta\ngamma\n")
    );
    assert!(host.buffer_version(path).unwrap() > v_before);

    rt.shutdown().expect("shutdown");
}

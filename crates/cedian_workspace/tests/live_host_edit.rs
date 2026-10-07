//! Phase 4 live smoke: agent edits a workspace buffer through the host side.
//!
//! Plan acceptance (headless): OMP `edit` intent routes out via
//! `cedian_apply_edit` host tool → buffer transaction → version bump; native
//! undo restores. Proves the §10 boundary live: model calls the host tool
//! (not the filesystem), cedian owns the transaction.
//!
//! Requires ambient OMP auth. Ignored by default:
//! `cargo test -p cedian_workspace -- --ignored --nocapture live_host_edit`

use cedian_omp::{OmpBinary, OmpRuntime, RuntimeConfig};
use cedian_workspace::{HostTools, WorkspaceHost};
use std::path::Path;
use std::time::Duration;

fn dev_config(tag: &str) -> RuntimeConfig {
    RuntimeConfig {
        binary: OmpBinary::Path("omp".to_string()),
        session_dir: std::env::temp_dir()
            .join(format!("cedian-phase4-{tag}-{}", std::process::id())),
        sessions: cedian_omp::Sessions::InSessionDir,
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
fn live_host_edit() {
    let host = HostTools::shared(Path::new("/"));
    // Deterministic buffer: the model edits this exact text.
    host.open(Path::new("/note.txt"), "version one");
    assert_eq!(host.buffer_version(Path::new("/note.txt")).unwrap().0, 0);

    let mut rt = OmpRuntime::spawn(dev_config("edit")).expect("spawn");
    rt.set_host_tools(vec![host.apply_edit_tool()])
        .expect("host tools");
    rt.set_host_uris(vec![host.cedian_uri_scheme()])
        .expect("host uris");

    // Agent Sync ON (§13): nothing dirty yet.
    assert!(host.agent_sync().is_empty());

    let turn = rt
        .prompt(
            "Call cedian_apply_edit with path '/note.txt', expected_version 0, start 8, end 11, \
             replacement 'two'. Then reply with only the tool result text.",
            vec![],
        )
        .expect("prompt");
    let text = turn.assistant_text.as_deref().unwrap_or("");
    assert!(
        text.contains("version 1"),
        "tool result echoed, got {text:?}"
    );

    // Transaction landed: text replaced, version bumped, dirty.
    assert_eq!(
        host.read_buffer(Path::new("/note.txt")).as_deref(),
        Some("version two")
    );
    assert_eq!(host.buffer_version(Path::new("/note.txt")).unwrap().0, 1);

    // Native undo restores (new version — undo is a change, not a rewind).
    let v = host.undo(Path::new("/note.txt")).unwrap();
    assert_eq!(
        host.read_buffer(Path::new("/note.txt")).as_deref(),
        Some("version one")
    );
    assert_eq!(v.0, 2);

    rt.shutdown().expect("shutdown");
}

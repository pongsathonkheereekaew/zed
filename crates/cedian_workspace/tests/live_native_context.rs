//! Phase 6 live smoke: highlight → "fix this" hits the right target.
//!
//! Plan acceptance: "Highlight code → say 'fix this' → OMP operates on the
//! correct target." Sets selection + active file + a diagnostic, sends the
//! ambient snapshot with the prompt; the model reads `cedian://selection` and
//! `cedian://diagnostics`, then fixes via `cedian_apply_edit`.
//!
//! Requires ambient OMP auth. Ignored by default:
//! `cargo test -p cedian_workspace -- --ignored --nocapture live_native_context`

use cedian_omp::{OmpBinary, OmpRuntime, RuntimeConfig};
use cedian_workspace::{
    Diagnostic, DiagnosticSeverity, HostTools, WorkspaceHost, capture_ambient, render_snapshot,
};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn dev_config(tag: &str) -> RuntimeConfig {
    RuntimeConfig {
        binary: OmpBinary::Path("omp".to_string()),
        session_dir: std::env::temp_dir()
            .join(format!("cedian-phase6-{tag}-{}", std::process::id())),
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
fn live_native_context() {
    let host = HostTools::shared(Path::new("/"));
    // "Highlighted" code: selection covers the buggy line.
    let code = "fn total(items: &[i32]) -> i32 {\n    let mut sum = 0;\n    sum\n}\n";
    host.open(Path::new("/shop.rs"), code);
    host.set_active_file(Some(PathBuf::from("/shop.rs")));
    // Byte offsets of "    sum\n" (line 3, 0-based line 2).
    let sel_start = code.find("    sum\n").unwrap();
    host.set_selection(Some((PathBuf::from("/shop.rs"), sel_start, sel_start + 8)));
    host.publish_diagnostics(
        Path::new("/shop.rs"),
        vec![Diagnostic {
            path: PathBuf::from("/shop.rs"),
            line: 2,
            severity: DiagnosticSeverity::Warning,
            message: "unused variable `sum` — did you mean to accumulate?".to_string(),
        }],
    );
    let mut rt = OmpRuntime::spawn(dev_config("ctx")).expect("spawn");
    rt.set_host_tools(vec![host.apply_edit_tool()])
        .expect("host tools");
    rt.set_host_uris(vec![host.cedian_uri_scheme()])
        .expect("host uris");

    // Ambient snapshot travels WITH the prompt (plan §39).
    let snapshot = render_snapshot(&capture_ambient(host.as_ref()));
    assert!(snapshot.contains("active-file: /shop.rs"));
    assert!(snapshot.contains("unused variable"));

    let prompt = format!(
        "{snapshot}\n\
         The user highlighted code and says 'fix this'. The full file body is at \
         cedian://buffer//shop.rs — read it, then fix the unused `sum` on line 3 \
         with cedian_apply_edit (expected_version 0) so it accumulates. \
         Reply with only: fixed-ok"
    );
    let turn = rt.prompt(&prompt, vec![]).expect("prompt");
    assert_eq!(turn.assistant_text.as_deref(), Some("fixed-ok"));

    // The fix landed in the buffer: selected line changed, still coherent.
    let after = host.read_buffer(Path::new("/shop.rs")).unwrap();
    assert_ne!(after, code, "buffer changed");
    assert!(after.contains("sum"), "still mentions sum, got:\n{after}");

    rt.shutdown().expect("shutdown");
}

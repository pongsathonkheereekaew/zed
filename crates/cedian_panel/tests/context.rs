//! S9 U6: OMP gets what the person sees from Zed. A workspace with
//! `src/main.rs` open in an editor, `let unused = 1;` selected, and a
//! warning Zed's language server published on that line. The real panel
//! sends a prompt; fake-omp, replaying `context.jsonl` through the real spawn
//! profile, checks:
//!
//! 1. the prompt message is the bounded snapshot (active file, selection,
//!    the diagnostic) and then the typed text (ARCHITECTURE §39);
//! 2. mid-turn it reads `cedian://diagnostics` and `cedian://selection`,
//!    and the panel answers from Zed what the recording answered (§40).
//!
//! fake-omp exits on any difference, which fails the turn.
//!
//! Harness off: invoked with `--mode` (or `config`) this binary is fake-omp.

use cedian_panel::{CedianPanel, Connection, Turn};
use editor::{Editor, MultiBufferOffset, SelectionEffects};
use gpui::{AppContext as _, Entity, TestAppContext};
use language::{Diagnostic, DiagnosticEntry, DiagnosticSeverity, DiagnosticSourceKind};
use lsp::LanguageServerId;
use project::Project;
use settings::SettingsStore;
use std::path::Path;
use std::time::{Duration, Instant};
use text::{PointUtf16, Unclipped};
use workspace::Workspace;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/context.jsonl");
const MAIN: &str = "fn main() {\n    let unused = 1;\n}\n";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--mode") => std::process::exit(cedian_fake_omp::run(&args)),
        Some("config") => std::process::exit(cedian_fake_omp::config_get(&args)),
        _ => {}
    }
    let root = std::env::temp_dir().join(format!("cedian-u6-context-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("ws/src")).unwrap();
    std::fs::write(root.join("ws/src/main.rs"), MAIN).unwrap();
    std::fs::write(root.join("cedian.toml"), "schema = 1\n").unwrap();
    let root = root.canonicalize().unwrap();
    // SAFETY: single-threaded here; nothing else reads the environment yet.
    unsafe {
        std::env::set_var("CEDIAN_CONFIG", root.join("cedian.toml"));
        std::env::set_var("CEDIAN_STATE_DIR", root.join("state"));
        std::env::set_var("HOME", root.join("home"));
        std::env::set_var("CEDIAN_OMP_BINARY", std::env::current_exe().unwrap());
    }
    print!("test selection_and_diagnostic_reach_omp ... ");
    gpui::run_test_once(
        0,
        Box::new(move |dispatcher| {
            let exec = std::sync::Arc::new(dispatcher.clone());
            let mut cx = TestAppContext::build(dispatcher.clone(), Some("context"));
            gpui::ForegroundExecutor::new(exec).block_test(scenario(&mut cx, &root));
            cx.run_until_parked();
            cx.update(|cx| cx.quit());
            cx.run_until_parked();
            dispatcher.drain_tasks();
            let _ = std::fs::remove_dir_all(&root);
        }),
    );
    println!("ok");
}

async fn scenario(cx: &mut TestAppContext, root: &Path) {
    cx.executor().allow_parking();
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
    });
    let ws = root.join("ws");
    let state = cedian_shell::state::dir(&ws).unwrap();
    cedian_fake_omp::install_replay(&state.join("omp"), Path::new(FIXTURE)).unwrap();

    let project = Project::test(fs::RealFs::new(None, cx.executor()), [ws.as_path()], cx).await;
    let main_rs = ws.join("src/main.rs");
    let buffer = project
        .update(cx, |p, cx| p.open_local_buffer(&main_rs, cx))
        .await
        .unwrap();
    project.update(cx, |p, cx| {
        p.lsp_store().update(cx, |store, cx| {
            store
                .update_diagnostic_entries(
                    LanguageServerId(0),
                    main_rs.clone(),
                    None,
                    None,
                    vec![DiagnosticEntry::new(
                        Unclipped(PointUtf16::new(1, 8))..Unclipped(PointUtf16::new(1, 14)),
                        Diagnostic {
                            severity: DiagnosticSeverity::WARNING,
                            message: "unused variable: `unused`".into(),
                            source_kind: DiagnosticSourceKind::Pushed,
                            is_primary: true,
                            ..Default::default()
                        },
                    )],
                    cx,
                )
                .unwrap();
        })
    });
    let window = cx.add_window(|window, cx| Workspace::test_new(project.clone(), window, cx));
    let panel = window
        .update(cx, |workspace, window, cx| {
            let editor =
                cx.new(|cx| Editor::for_buffer(buffer.clone(), Some(project.clone()), window, cx));
            editor.update(cx, |editor, cx| {
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
                    s.select_ranges([MultiBufferOffset(16)..MultiBufferOffset(31)])
                });
            });
            workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
            let handle = cx.entity().downgrade();
            cx.new(|cx| {
                let mut panel = CedianPanel::new(project.clone(), window, cx);
                panel.set_workspace(handle);
                panel
            })
        })
        .unwrap();
    wait(cx, &panel, "OMP ready", |p| {
        matches!(p.connection(), Connection::Ready { .. })
    });
    // Not through the workspace's own update: the panel reads the workspace.
    cx.update_window(window.into(), |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.set_prompt("Fix this", window, cx);
            panel.submit(window, cx);
        })
    })
    .unwrap();
    wait(cx, &panel, "the turn to end", |p| p.turn() == &Turn::Idle);
    let (connection, notice) = panel.read_with(cx, |p, _| {
        (p.connection().clone(), p.notice().map(str::to_string))
    });
    assert!(
        matches!(connection, Connection::Ready { .. }) && notice.is_none(),
        "fake-omp replayed to the end (no divergence): {connection:?} · {notice:?}"
    );
}

/// Pump the test executor while real OS threads (OMP) make progress.
fn wait(
    cx: &mut TestAppContext,
    panel: &Entity<CedianPanel>,
    what: &str,
    done: impl Fn(&CedianPanel) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        cx.run_until_parked();
        let (finished, state) = panel.read_with(cx, |p, _| {
            (
                done(p),
                format!("{:?} · {:?} · {:?}", p.connection(), p.turn(), p.notice()),
            )
        });
        if finished {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {state}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

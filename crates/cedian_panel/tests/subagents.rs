//! S9 U8 (ADR-0050): OMP subagents in the app. The real panel, OMP played
//! by fake-omp replaying `subagents.jsonl` through the real spawn profile.
//!
//! 1. The link subscribes at level `progress` before the session opens
//!    (the fixture's preamble has the request; without it replay diverges);
//!    a `task` call's subagents render under its tool card, running;
//! 2. as OMP ends them their rows show completed and failed.
//!
//! Harness off: invoked with `--mode` (or `config`) this binary is fake-omp.

use cedian_omp::SubagentStatus;
use cedian_panel::{CedianPanel, Connection, Turn};
use gpui::{TestAppContext, VisualTestContext, WindowHandle};
use project::Project;
use settings::SettingsStore;
use std::path::Path;
use std::time::{Duration, Instant};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/subagents.jsonl"
);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--mode") => std::process::exit(cedian_fake_omp::run(&args)),
        Some("config") => std::process::exit(cedian_fake_omp::config_get(&args)),
        _ => {}
    }
    let root = std::env::temp_dir().join(format!("cedian-u8-subagents-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("ws")).unwrap();
    std::fs::write(root.join("cedian.toml"), "schema = 1\n").unwrap();
    let root = root.canonicalize().unwrap();
    // SAFETY: single-threaded here; nothing else reads the environment yet.
    unsafe {
        std::env::set_var("CEDIAN_CONFIG", root.join("cedian.toml"));
        std::env::set_var("CEDIAN_STATE_DIR", root.join("state"));
        std::env::set_var("HOME", root.join("home"));
        std::env::set_var("CEDIAN_OMP_BINARY", std::env::current_exe().unwrap());
    }
    print!("test subagents ... ");
    gpui::run_test_once(
        0,
        Box::new(move |dispatcher| {
            let exec = std::sync::Arc::new(dispatcher.clone());
            let mut cx = TestAppContext::build(dispatcher.clone(), Some("subagents"));
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
    std::fs::write(ws.join("session.jsonl"), "").unwrap();
    let state = cedian_shell::state::dir(&ws).unwrap();
    cedian_fake_omp::install_replay(&state.join("omp"), Path::new(FIXTURE)).unwrap();

    let project = Project::test(fs::RealFs::new(None, cx.executor()), [ws.as_path()], cx).await;
    let window = cx.add_window(|window, cx| CedianPanel::new(project.clone(), window, cx));
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    wait(cx, &window, "OMP ready", |p| {
        matches!(p.connection(), Connection::Ready { .. })
    });

    // 1. A task call's subagents, running, under its card.
    submit(cx, &window, "fan out");
    wait(cx, &window, "two running subagents", |p| {
        statuses(p)
            == [
                ("sa-1", SubagentStatus::Running),
                ("sa-2", SubagentStatus::Running),
            ]
    });
    let card = rendered(&mut vcx, "cedian-tool-call-task").expect("the task card renders");
    for id in ["sa-1", "sa-2"] {
        let row = rendered(&mut vcx, selector(id)).unwrap_or_else(|| panic!("{id} renders"));
        assert!(
            row.origin.y > card.origin.y,
            "{id} sits under the task card"
        );
    }
    let transcript = window.update(cx, |p, _, _| p.transcript()).unwrap();
    let task = transcript
        .iter()
        .position(|l| l.contains("Subagent task"))
        .unwrap_or_else(|| panic!("{transcript:?}"));
    assert!(
        transcript[task + 1].contains("explore")
            && transcript[task + 1].contains("reading event_router.rs"),
        "{transcript:?}"
    );

    // 2. OMP ends them.
    window.update(cx, |p, _, cx| p.stop_turn(cx)).unwrap();
    wait(cx, &window, "the subagents to end", |p| {
        p.turn() == &Turn::Idle
            && statuses(p)
                == [
                    ("sa-1", SubagentStatus::Completed),
                    ("sa-2", SubagentStatus::Failed),
                ]
    });
    assert_connected(cx, &window);
}

fn selector(id: &str) -> &'static str {
    match id {
        "sa-1" => "cedian-subagent-sa-1",
        "sa-2" => "cedian-subagent-sa-2",
        _ => unreachable!(),
    }
}

fn statuses(p: &CedianPanel) -> Vec<(&str, SubagentStatus)> {
    p.subagents()
        .rows()
        .iter()
        .map(|r| (r.id.as_str(), r.status))
        .collect()
}

fn submit(cx: &mut TestAppContext, window: &WindowHandle<CedianPanel>, text: &str) {
    window
        .update(cx, |panel, window, cx| {
            panel.set_prompt(text, window, cx);
            panel.submit(window, cx);
        })
        .unwrap();
}

fn rendered(
    vcx: &mut VisualTestContext,
    selector: &'static str,
) -> Option<gpui::Bounds<gpui::Pixels>> {
    vcx.update(|window, _| window.refresh());
    vcx.run_until_parked();
    vcx.debug_bounds(selector)
}

fn assert_connected(cx: &mut TestAppContext, window: &WindowHandle<CedianPanel>) {
    let connection = window.update(cx, |p, _, _| p.connection().clone()).unwrap();
    assert!(
        matches!(connection, Connection::Ready { .. }),
        "fake-omp still replaying (no divergence): {connection:?}"
    );
}

/// Pump the test executor while real OS threads (OMP) make progress.
fn wait(
    cx: &mut TestAppContext,
    window: &WindowHandle<CedianPanel>,
    what: &str,
    done: impl Fn(&CedianPanel) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        cx.run_until_parked();
        let (finished, state) = window
            .update(cx, |p, _, _| {
                (
                    done(p),
                    format!(
                        "{:?} · {:?} · {:?} · {:?}",
                        p.connection(),
                        p.turn(),
                        p.notice(),
                        p.subagents().rows()
                    ),
                )
            })
            .unwrap();
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

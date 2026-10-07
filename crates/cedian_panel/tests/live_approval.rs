//! S9 U4 exit check: a real OMP 18.6.1 turn, recorded once and replayed in
//! the app. The model runs `echo cedian` with bash; OMP raises its own
//! `Allow tool: bash` approval; the person clicks Approve in the panel; the
//! tool output and the final text show; the audit has allow/user/bash.
//!
//! Replay (default) plays `live_approval.jsonl` through fake-omp. Record
//! (`CEDIAN_U4_RECORD=1`, one model call, needs OMP auth) proxies to the
//! pinned OMP through a wrapper that adds `--session-dir` in the run's temp
//! dir, so the session stays out of the person's OMP store, and writes the
//! redacted fixture.
//!
//! Harness off: invoked with `--mode` (or `config`) this binary is fake-omp.

#![allow(
    clippy::disallowed_methods,
    reason = "test setup writes a wrapper script with std::fs"
)]

use cedian_panel::{CedianPanel, Connection, Turn};
use gpui::{Modifiers, TestAppContext, VisualTestContext, WindowHandle};
use omp_rpc::ExtensionUiRequest;
use project::Project;
use serde_json::Value;
use settings::SettingsStore;
use std::path::Path;
use std::time::{Duration, Instant};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/live_approval.jsonl"
);
const PROMPT: &str = "Run the shell command `echo cedian` with your bash tool. \
                      Then reply with exactly: done";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--mode") => std::process::exit(cedian_fake_omp::run(&args)),
        Some("config") => std::process::exit(cedian_fake_omp::config_get(&args)),
        _ => {}
    }
    let record = std::env::var_os("CEDIAN_U4_RECORD").is_some();
    let root = std::env::temp_dir().join(format!("cedian-u4-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("ws")).unwrap();
    std::fs::write(root.join("cedian.toml"), "schema = 1\n").unwrap();
    let root = root.canonicalize().unwrap();
    // SAFETY: single-threaded here; nothing else reads the environment yet.
    unsafe {
        std::env::set_var("CEDIAN_CONFIG", root.join("cedian.toml"));
        std::env::set_var("CEDIAN_STATE_DIR", root.join("state"));
        std::env::set_var("CEDIAN_OMP_BINARY", std::env::current_exe().unwrap());
    }
    print!("test live_approval_replays_in_the_app ... ");
    gpui::run_test_once(
        0,
        Box::new(move |dispatcher| {
            let exec = std::sync::Arc::new(dispatcher.clone());
            let mut cx = TestAppContext::build(dispatcher.clone(), Some("live_approval"));
            gpui::ForegroundExecutor::new(exec).block_test(scenario(&mut cx, &root, record));
            cx.run_until_parked();
            cx.update(|cx| cx.quit());
            cx.run_until_parked();
            dispatcher.drain_tasks();
            let _ = std::fs::remove_dir_all(&root);
        }),
    );
    println!("ok");
}

async fn scenario(cx: &mut TestAppContext, root: &Path, record: bool) {
    cx.executor().allow_parking();
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
    });
    let ws = root.join("ws");
    let state = cedian_shell::state::dir(&ws).unwrap();
    let sessions = state.join("omp");
    if record {
        let omp = std::env::var("CEDIAN_U4_OMP").expect("CEDIAN_U4_OMP: the pinned omp");
        let wrapper = root.join("omp-in-temp-store");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nexec '{omp}' \"$@\" --session-dir '{}'\n",
                sessions.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&wrapper).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&wrapper, permissions).unwrap();
        cedian_fake_omp::arm_record(&sessions, &wrapper).unwrap();
    } else {
        cedian_fake_omp::install_replay(&sessions, Path::new(FIXTURE)).unwrap();
    }

    let project = Project::test(fs::RealFs::new(None, cx.executor()), [ws.as_path()], cx).await;
    let window = cx.add_window(|window, cx| CedianPanel::new(project.clone(), window, cx));
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    wait(cx, &window, "OMP ready", |p| {
        matches!(p.connection(), Connection::Ready { .. })
    });

    window
        .update(cx, |panel, window, cx| {
            panel.set_prompt(PROMPT, window, cx);
            panel.submit(window, cx);
        })
        .unwrap();
    wait(cx, &window, "OMP's approval", |p| {
        !p.dialog_ids().is_empty()
    });
    let (id, approve) = window
        .update(cx, |p, _, _| {
            let id = p.dialog_ids().remove(0);
            let request = p.dialog(&id).unwrap().request().clone();
            let ExtensionUiRequest::Select(select) = request else {
                panic!("not an approval select: {request:?}");
            };
            assert!(
                select.title.starts_with("Allow tool: bash"),
                "{}",
                select.title
            );
            let approve = select
                .options
                .iter()
                .find(|o| o.to_ascii_lowercase().starts_with("approve"))
                .unwrap_or_else(|| panic!("no Approve in {:?}", select.options))
                .clone();
            (id, approve)
        })
        .unwrap();
    // debug_bounds takes a &'static str; this id is only known at run time.
    let button: &'static str = format!("cedian-dialog-{id}-{approve}").leak();
    click(&mut vcx, button);
    wait(cx, &window, "the approved turn to complete", |p| {
        p.dialog_ids().is_empty() && p.turn() == &Turn::Idle
    });

    if record {
        let recorded =
            std::fs::read_to_string(sessions.join(cedian_fake_omp::RECORDED_FILE)).unwrap();
        std::fs::write(FIXTURE, recorded).unwrap();
        eprintln!("fixture written: {FIXTURE}");
    }
    let transcript = window.update(cx, |p, _, _| p.transcript()).unwrap();
    assert!(
        transcript
            .iter()
            .any(|l| l.contains("[Done]") && l.contains("echo cedian") && l.contains("→ cedian")),
        "the bash card with its output: {transcript:#?}"
    );
    assert!(
        transcript
            .iter()
            .any(|l| l.starts_with("Assistant") && l.contains("done")),
        "the final text: {transcript:#?}"
    );
    let gates: Vec<Value> = std::fs::read_to_string(state.join("audit.jsonl"))
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|row| row["item"].clone())
        .filter(|item| item["kind"] == "gate")
        .collect();
    assert_eq!(gates.len(), 1, "{gates:?}");
    assert_eq!(
        (
            gates[0]["decision"].as_str(),
            gates[0]["answered_by"].as_str(),
            gates[0]["tool"].as_str()
        ),
        (Some("allow"), Some("user"), Some("bash")),
    );
    assert!(matches!(
        window.update(cx, |p, _, _| p.connection().clone()).unwrap(),
        Connection::Ready { .. }
    ));
}

fn rendered(
    vcx: &mut VisualTestContext,
    selector: &'static str,
) -> Option<gpui::Bounds<gpui::Pixels>> {
    vcx.update(|window, _| window.refresh());
    vcx.run_until_parked();
    vcx.debug_bounds(selector)
}

fn click(vcx: &mut VisualTestContext, selector: &'static str) {
    let bounds = rendered(vcx, selector).unwrap_or_else(|| panic!("{selector} is not on screen"));
    vcx.simulate_click(bounds.center(), Modifiers::none());
}

fn wait(
    cx: &mut TestAppContext,
    window: &WindowHandle<CedianPanel>,
    what: &str,
    done: impl Fn(&CedianPanel) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        cx.run_until_parked();
        let (finished, state) = window
            .update(cx, |p, _, _| {
                (
                    done(p),
                    format!(
                        "{:?} · {:?} · {:?}",
                        p.connection(),
                        p.turn(),
                        p.dialog_ids()
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
        std::thread::sleep(Duration::from_millis(100));
    }
}

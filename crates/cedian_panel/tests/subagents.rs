//! S9 U8 (ADR-0050): OMP subagents in the app. The real panel, OMP played
//! by fake-omp replaying `subagents.jsonl` through the real spawn profile.
//!
//! 1. The link subscribes at level `progress` before the session opens
//!    (the fixture's preamble has the request; without it replay diverges);
//!    a `task` call's subagents render under its tool card, running;
//! 2. Steer on a row sends `steer_subagent` with that id and text (replay
//!    checks both);
//! 3. a steer OMP refuses ("Subagent not running") shows on the row;
//! 4. Cancel on a subagent that had already ended: OMP says
//!    `cancelled: false`, the row says "already ended";
//! 5. Cancel on a running one: OMP aborts it, the row shows aborted, and
//!    each Cancel is an audit row;
//! 6. the link registers `cedian_worktree_request`: a request without the
//!    ADR-0033 brief is refused naming the missing fields, with no tree;
//!    one with it gets a tree, a registry row and its stored brief.
//!
//! Harness off: invoked with `--mode` (or `config`) this binary is fake-omp.

#![allow(
    clippy::disallowed_methods,
    reason = "test setup runs git to make the workspace a repository"
)]

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
    std::fs::write(ws.join("notes.txt"), "base\n").unwrap();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["config", "user.email", "t@t"],
        &["config", "user.name", "t"],
        &["add", "notes.txt"],
        &["commit", "-q", "-m", "base"],
    ] {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(&ws)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_WORK_TREE")
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }
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

    // 2. Steer one through its row: replay checks the id and the text.
    type_steer(cx, &window, "sa-1", "focus on routing");
    click(&mut vcx, "cedian-subagent-sa-1-steer");
    wait(cx, &window, "the steer to land", |p| {
        p.subagents().get("sa-1").unwrap().description == "routing only"
    });
    assert_connected(cx, &window);

    // 3. A steer OMP refuses shows on the row.
    type_steer(cx, &window, "sa-2", "and the tests");
    click(&mut vcx, "cedian-subagent-sa-2-steer");
    wait(cx, &window, "the refusal on the row", |p| {
        p.subagent_note("sa-2")
            .is_some_and(|n| n.contains("Subagent not running: sa-2"))
    });

    // 4. Cancel one that had already ended: "already ended", audited.
    click(&mut vcx, "cedian-subagent-sa-2-cancel");
    wait(cx, &window, "already ended", |p| {
        p.subagent_note("sa-2") == Some("already ended")
            && p.subagents().get("sa-2").unwrap().status == SubagentStatus::Completed
    });

    // 5. Cancel a running one: OMP aborts it; the turn ends once.
    click(&mut vcx, "cedian-subagent-sa-1-cancel");
    wait(
        cx,
        &window,
        "the cancelled subagent and the turn's end",
        |p| {
            p.turn() == &Turn::Idle
                && statuses(p)
                    == [
                        ("sa-1", SubagentStatus::Aborted),
                        ("sa-2", SubagentStatus::Completed),
                    ]
        },
    );
    assert!(
        rendered(&mut vcx, "cedian-subagent-sa-1-cancel").is_none(),
        "an ended subagent offers no Cancel"
    );
    assert_connected(cx, &window);
    let rows: Vec<(String, bool)> = std::fs::read_to_string(state.join("audit.jsonl"))
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .map(|row| row["item"].clone())
        .filter(|item| item["tool"] == "cancel_subagent")
        .map(|item| {
            (
                item["command"].as_str().unwrap().to_string(),
                item["cancelled"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [("sa-2".to_string(), false), ("sa-1".to_string(), true)]
    );

    // 6. OMP asks the app for worktrees: one without the ADR-0033 brief is
    // refused naming the missing fields (replay checks the reason) and no
    // tree appears; one with the brief gets its tree, row and brief.
    submit(cx, &window, "make trees");
    wait(cx, &window, "the worktree turn", |p| {
        p.turn() == &Turn::Idle && p.transcript().iter().any(|l| l == "User: make trees")
    });
    assert_connected(cx, &window);
    assert!(
        !ws.join(".worktrees/bare").exists(),
        "a brief-less request makes no tree"
    );
    assert!(
        ws.join(".worktrees/w1/notes.txt").exists(),
        "the briefed tree"
    );
    let registry = std::fs::read_to_string(state.join("workers.json")).unwrap();
    assert!(
        registry.contains("fix the casing") && !registry.contains("bare"),
        "{registry}"
    );
    assert!(state.join("briefs/w1.1.json").exists());
}

fn type_steer(cx: &mut TestAppContext, window: &WindowHandle<CedianPanel>, id: &str, text: &str) {
    window
        .update(cx, |p, window, cx| {
            let input = p
                .subagent_steer_box(id)
                .expect("a running row has a steer box");
            input.update(cx, |editor, cx| editor.set_text(text, window, cx));
        })
        .unwrap();
}

fn click(vcx: &mut VisualTestContext, selector: &'static str) {
    let bounds = rendered(vcx, selector).unwrap_or_else(|| panic!("{selector} is not on screen"));
    vcx.simulate_click(bounds.center(), gpui::Modifiers::none());
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

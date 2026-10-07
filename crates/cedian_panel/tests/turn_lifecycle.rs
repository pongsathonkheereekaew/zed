//! S9 U4: one prompt at a time, and every way a prompt ends reaches the
//! panel. The real panel, OMP played by fake-omp replaying four fixtures,
//! one per OMP process (`turn_lifecycle_{1,2,3,4}.jsonl`):
//!
//! 1. Stop while the prompt is still queued: it never reaches OMP (the
//!    fixture's first prompt is "one", and replay checks the text);
//! 2. Send while a turn runs is refused, so a second prompt never streams
//!    as idle; Stop aborts the turn;
//! 3. a prompt OMP rejects returns the panel to idle with the reason;
//! 4. an answer OMP got but the audit lost fails the turn with exactly one
//!    abort (a second would meet the next recorded prompt and end the
//!    replay);
//! 5. Restart with a prompt still queued: the old OMP never sends it (the
//!    fixture would write `old-omp-got-four`), though it was alive and held
//!    the prompt in its queue at the restart (its thread parked by the
//!    test until the restart has closed the old link);
//! 6. Stop, then Restart while the old OMP is slow to exit and still holds
//!    the session file: one abort, and the new OMP waits for the old and
//!    opens the session, not Taken;
//! 7. Restart while the old OMP never answers the abort and a process it
//!    started holds the session file: the old OMP's group is killed and the
//!    new OMP opens the session, not Taken.
//!
//! Harness off: invoked with `--mode` (or `config`) this binary is fake-omp.

#![allow(
    clippy::disallowed_methods,
    reason = "probing a real OS process id; no async spawn helper applies"
)]

use cedian_omp::UserAnswer;
use cedian_panel::{CedianPanel, Connection, Turn};
use gpui::{TestAppContext, WindowHandle};
use project::Project;
use settings::SettingsStore;
use std::path::Path;
use std::time::{Duration, Instant};

fn fixture(n: u8) -> String {
    format!(
        "{}/tests/fixtures/turn_lifecycle_{n}.jsonl",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--mode") => std::process::exit(cedian_fake_omp::run(&args)),
        Some("config") => std::process::exit(cedian_fake_omp::config_get(&args)),
        _ => {}
    }
    let root = std::env::temp_dir().join(format!("cedian-u4-turns-{}", std::process::id()));
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
    print!("test turn_lifecycle ... ");
    gpui::run_test_once(
        0,
        Box::new(move |dispatcher| {
            let exec = std::sync::Arc::new(dispatcher.clone());
            let mut cx = TestAppContext::build(dispatcher.clone(), Some("turn_lifecycle"));
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
    let sessions = cedian_shell::state::dir(&ws).unwrap().join("omp");
    let install = |n| cedian_fake_omp::install_replay(&sessions, Path::new(&fixture(n))).unwrap();
    install(1);

    let project = Project::test(fs::RealFs::new(None, cx.executor()), [ws.as_path()], cx).await;
    let window = cx.add_window(|window, cx| CedianPanel::new(project.clone(), window, cx));

    // 1. Stop while queued, before OMP is even ready.
    let turn = window
        .update(cx, |panel, window, cx| {
            panel.set_prompt("never", window, cx);
            panel.submit(window, cx);
            assert_eq!(panel.turn(), &Turn::Queued);
            panel.stop_turn(cx);
            panel.turn().clone()
        })
        .unwrap();
    assert_eq!(turn, Turn::Stopping);
    wait(cx, &window, "the cancelled prompt to end", |p| {
        ready(p) && p.turn() == &Turn::Idle
    });
    let transcript = window.update(cx, |p, _, _| p.transcript()).unwrap();
    assert!(
        !transcript.iter().any(|l| l.contains("never")),
        "OMP never ran it: {transcript:?}"
    );

    // 2. One turn at a time.
    assert_eq!(submit(cx, &window, "one").0, Turn::Queued);
    wait(cx, &window, "turn one to stream", |p| {
        p.turn() == &Turn::Streaming
    });
    let (turn, notice) = submit(cx, &window, "two");
    assert_eq!(turn, Turn::Streaming, "turn one still runs");
    assert!(notice.contains("a turn is running"), "{notice}");
    window.update(cx, |p, _, cx| p.stop_turn(cx)).unwrap();
    wait(cx, &window, "turn one to stop", |p| p.turn() == &Turn::Idle);
    assert_ready(cx, &window);

    // 3. OMP rejects the prompt.
    submit(cx, &window, "three");
    wait(cx, &window, "the rejected prompt", |p| {
        p.turn() == &Turn::Idle
            && p.notice()
                .is_some_and(|n| n.contains("rejected by the test"))
    });

    // 4. Unaudited answer: the turn fails, one abort.
    submit(cx, &window, "audit");
    wait(cx, &window, "the dialog", |p| {
        p.dialog_ids() == ["u4-audit"]
    });
    let turn = window
        .update(cx, |p, _, cx| {
            p.break_audit();
            p.answer("u4-audit", UserAnswer::Choice("Approve".into()), cx);
            p.turn().clone()
        })
        .unwrap();
    assert!(
        matches!(turn, Turn::Failed(ref r) if r.starts_with("audit:")),
        "{turn:?}"
    );
    wait(cx, &window, "the failed turn to settle", |p| {
        p.turn() == &Turn::Idle
    });
    let notice = window
        .update(cx, |p, _, _| p.notice().map(str::to_string))
        .unwrap();
    assert!(notice.is_some_and(|n| n.starts_with("turn failed: audit")));
    submit(cx, &window, "after the audit");
    wait(cx, &window, "the turn after the audit", |p| {
        p.turn() == &Turn::Idle && p.notice().is_none()
    });
    assert_ready(cx, &window);

    // 5. Restart with a prompt queued behind a parked OMP thread. The
    // thread is released only once Restart has dropped the old link, so
    // the prompt is in the queue at the restart whatever the machine's
    // speed (a driver check as the lever lost that race on CI).
    install(2);
    let old = window
        .update(cx, |panel, window, cx| {
            let hold = panel.hold_omp();
            panel.set_prompt("four", window, cx);
            panel.submit(window, cx);
            assert!(panel.prompt_queued(), "the old OMP holds four in its queue");
            let old = panel.omp_pid().unwrap();
            assert!(alive(old), "the old OMP runs");
            panel.restart(window, cx);
            drop(hold);
            old
        })
        .unwrap();
    wait(cx, &window, "the restarted OMP", ready);
    assert!(
        !ws.join("old-omp-got-four").exists(),
        "the old OMP sent the queued prompt"
    );

    assert!(!alive(old), "the old OMP ended");

    // 6. Stop, then Restart while the old OMP winds down holding the
    // session file: one abort (fixture 2 records a second as a file).
    submit(cx, &window, "slow");
    wait(cx, &window, "the slow turn", |p| {
        p.turn() == &Turn::Streaming
    });
    install(3);
    window
        .update(cx, |panel, window, cx| {
            panel.stop_turn(cx);
            panel.restart(window, cx);
        })
        .unwrap();
    wait(cx, &window, "the session after the slow OMP", |p| {
        !matches!(p.connection(), Connection::Starting)
    });
    assert_ready(cx, &window);
    assert!(
        !ws.join("old-omp-got-two-aborts").exists(),
        "Stop then Restart sent a second abort"
    );
    submit(cx, &window, "five");
    wait(cx, &window, "turn five", |p| {
        p.turn() == &Turn::Idle && p.notice().is_none()
    });
    assert_ready(cx, &window);

    // 7. Restart while the old OMP never answers the abort and a process
    // it started holds the session file: past the wait, its whole group is
    // killed and the new OMP opens the session.
    submit(cx, &window, "hang");
    wait(cx, &window, "the hanging turn", |p| {
        p.turn() == &Turn::Streaming
    });
    install(4);
    cedian_panel::omp_link::set_previous_exit(Duration::from_millis(500));
    window
        .update(cx, |panel, window, cx| panel.restart(window, cx))
        .unwrap();
    wait(cx, &window, "the session after the hung OMP", |p| {
        !matches!(p.connection(), Connection::Starting)
    });
    assert_ready(cx, &window);
}

/// Whether `pid` runs: a zombie, dead but not yet reaped, does not.
fn alive(pid: u32) -> bool {
    std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|out| {
            let stat = String::from_utf8_lossy(&out.stdout);
            out.status.success() && !stat.trim().is_empty() && !stat.trim().starts_with('Z')
        })
}

fn ready(p: &CedianPanel) -> bool {
    matches!(p.connection(), Connection::Ready { .. })
}

fn assert_ready(cx: &mut TestAppContext, window: &WindowHandle<CedianPanel>) {
    let connection = window.update(cx, |p, _, _| p.connection().clone()).unwrap();
    assert!(
        matches!(connection, Connection::Ready { .. }),
        "{connection:?}"
    );
}

fn submit(
    cx: &mut TestAppContext,
    window: &WindowHandle<CedianPanel>,
    text: &str,
) -> (Turn, String) {
    window
        .update(cx, |panel, window, cx| {
            panel.set_prompt(text, window, cx);
            panel.submit(window, cx);
            (
                panel.turn().clone(),
                panel.notice().unwrap_or_default().to_string(),
            )
        })
        .unwrap()
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
                    format!("{:?} · {:?} · {:?}", p.connection(), p.turn(), p.notice()),
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

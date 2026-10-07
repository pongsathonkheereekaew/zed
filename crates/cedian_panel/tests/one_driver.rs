//! S9 U4, ADR-0040 decision 5: one driver per session. The real panel, OMP
//! played by fake-omp replaying `one_driver.jsonl`; a child process holding
//! the session file open stands in for another OMP driving it.
//!
//! 1. OMP resumes a session another process holds: the panel says so,
//!    offers "Start a new session" and "Retry", and refuses prompts;
//! 2. Retry while it is still held changes nothing; once the holder exits,
//!    Retry opens the session;
//! 3. a holder that appears later stops the next prompt before OMP sees it;
//! 4. "Start a new session" moves to a fresh session, which takes prompts.
//!
//! A prompt that reached OMP on the taken session would meet fake-omp's
//! recorded `new_session` and end the replay.
//!
//! Harness off: invoked with `--mode` (or `config`) this binary is fake-omp.

#![allow(
    clippy::disallowed_methods,
    reason = "the stand-in driver is a real OS process holding a file; no async spawn helper applies"
)]

use cedian_panel::{CedianPanel, Connection, Turn};
use gpui::{Modifiers, TestAppContext, VisualTestContext, WindowHandle};
use project::Project;
use settings::SettingsStore;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/one_driver.jsonl"
);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--mode") => std::process::exit(cedian_fake_omp::run(&args)),
        Some("config") => std::process::exit(cedian_fake_omp::config_get(&args)),
        _ => {}
    }
    let root = std::env::temp_dir().join(format!("cedian-u4-driver-{}", std::process::id()));
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
    print!("test one_driver_per_session ... ");
    gpui::run_test_once(
        0,
        Box::new(move |dispatcher| {
            let exec = std::sync::Arc::new(dispatcher.clone());
            let mut cx = TestAppContext::build(dispatcher.clone(), Some("one_driver"));
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

/// Another process with the session file open for writing, as a driving
/// OMP keeps it.
fn hold(file: &Path) -> Child {
    Command::new("/bin/sh")
        .arg("-c")
        .arg("exec 3>>\"$0\"; exec sleep 120")
        .arg(file)
        .spawn()
        .unwrap()
}

/// Wait until `holder` has `file` open, as lsof sees it.
fn until_held(file: &Path, holder: &Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cedian_omp::driver::holders(&[file.to_path_buf()])
        .unwrap()
        .contains(&holder.id())
    {
        assert!(
            Instant::now() < deadline,
            "the holder never opened {file:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn release(mut holder: Child) {
    holder.kill().ok();
    holder.wait().ok();
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
    let sessions = cedian_shell::state::dir(&ws).unwrap().join("omp");
    cedian_fake_omp::install_replay(&sessions, Path::new(FIXTURE)).unwrap();
    let held = sessions.join("held.jsonl");
    std::fs::write(&held, "").unwrap();
    let holder = hold(&held);
    until_held(&held, &holder);

    let project = Project::test(fs::RealFs::new(None, cx.executor()), [ws.as_path()], cx).await;
    let window = cx.add_window(|window, cx| CedianPanel::new(project.clone(), window, cx));
    let mut vcx = VisualTestContext::from_window(window.into(), cx);

    // 1. Taken on open.
    wait(cx, &window, "the taken session", |p| {
        matches!(p.connection(), Connection::Taken { .. })
    });
    let connection = window.update(cx, |p, _, _| p.connection().clone()).unwrap();
    assert!(
        matches!(&connection, Connection::Taken { reason, .. } if reason.contains(&holder.id().to_string())),
        "{connection:?}"
    );
    for button in ["cedian-new-session", "cedian-retry"] {
        assert!(rendered(&mut vcx, button).is_some(), "{button}");
    }
    let (turn, notice) = submit(cx, &window, "refused");
    assert_eq!(turn, Turn::Idle);
    assert!(notice.contains("another process drives"), "{notice}");

    // 2. Retry: it checks again (the panel shows it checking) and still
    // finds the holder.
    click(&mut vcx, "cedian-retry");
    assert_eq!(
        window.update(cx, |p, _, _| p.connection().clone()).unwrap(),
        Connection::Checking,
        "Retry asked for a check"
    );
    wait(cx, &window, "Retry's check", |p| {
        matches!(p.connection(), Connection::Taken { .. })
    });
    release(holder);
    click(&mut vcx, "cedian-retry");
    wait(cx, &window, "the session to open", |p| {
        matches!(p.connection(), Connection::Ready { resumed: true, .. })
    });

    // 3. A driver appears before the next prompt.
    let holder = hold(&held);
    until_held(&held, &holder);
    let (turn, _) = submit(cx, &window, "must not reach OMP");
    assert_eq!(turn, Turn::Queued, "the panel did not know yet");
    wait(cx, &window, "the prompt to be refused", |p| {
        matches!(p.connection(), Connection::Taken { .. }) && p.turn() == &Turn::Idle
    });
    let transcript = window.update(cx, |p, _, _| p.transcript()).unwrap();
    assert!(
        !transcript.iter().any(|l| l.contains("must not reach OMP")),
        "a refused prompt is not shown as sent: {transcript:?}"
    );

    // 4. A new session.
    click(&mut vcx, "cedian-new-session");
    wait(cx, &window, "the new session", |p| {
        matches!(p.connection(), Connection::Ready { session_id, resumed: false, .. }
            if session_id.ends_with("0002"))
    });
    let (turn, _) = submit(cx, &window, "hello");
    assert_eq!(turn, Turn::Queued);
    wait(cx, &window, "the prompt on the new session", |p| {
        p.turn() == &Turn::Idle
    });
    assert!(matches!(
        window.update(cx, |p, _, _| p.connection().clone()).unwrap(),
        Connection::Ready { .. }
    ));
    release(holder);
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

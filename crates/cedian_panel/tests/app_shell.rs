//! S9 U3: the app starts OMP itself and survives it dying. The real panel in
//! a GPUI test context, OMP played by fake-omp through the real spawn
//! profile: launch → ready on a session; SIGKILL OMP → the panel shows why
//! and refuses prompts while the app keeps running; Restart → ready again on
//! the same session, resumed.
//!
//! Harness off: invoked with `--mode` (or `config`) this binary is fake-omp,
//! which is how the spawn profile, with its scrubbed env, reaches it.
#![allow(
    clippy::disallowed_methods,
    reason = "kills the OMP child with a real signal; no async spawn helper applies"
)]

use cedian_panel::{CedianPanel, Connection};
use gpui::{TestAppContext, WindowHandle};
use project::Project;
use settings::SettingsStore;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const LAUNCH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/app_launch.jsonl"
);
const RESUME: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/app_resume.jsonl"
);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--mode") => std::process::exit(cedian_fake_omp::run(&args)),
        Some("--version") => std::process::exit(cedian_fake_omp::version()),
        Some("config") => std::process::exit(cedian_fake_omp::config_get(&args)),
        _ => {}
    }
    let root = std::env::temp_dir().join(format!("cedian-u3-app-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("ws")).unwrap();
    std::fs::write(root.join("ws/notes.txt"), "alpha\n").unwrap();
    std::fs::write(root.join("cedian.toml"), "schema = 1\n").unwrap();
    let root = root.canonicalize().unwrap();
    // SAFETY: single-threaded here; nothing else reads the environment yet.
    unsafe {
        std::env::set_var("CEDIAN_CONFIG", root.join("cedian.toml"));
        std::env::set_var("CEDIAN_STATE_DIR", root.join("state"));
        std::env::set_var("HOME", root.join("home"));
        std::env::set_var("CEDIAN_OMP_BINARY", std::env::current_exe().unwrap());
    }
    print!("test app_shell_launch_crash_restart ... ");
    gpui::run_test_once(
        0,
        Box::new(move |dispatcher| {
            let exec = std::sync::Arc::new(dispatcher.clone());
            let mut cx = TestAppContext::build(dispatcher.clone(), Some("app_shell"));
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
    let session_dir = session_dir(&ws);
    cedian_fake_omp::install_replay(&session_dir, Path::new(LAUNCH)).unwrap();

    let project = Project::test(fs::RealFs::new(None, cx.executor()), [ws.as_path()], cx).await;
    let window = cx.add_window(|window, cx| CedianPanel::new(project.clone(), window, cx));

    // Launch: the panel opened on a folder starts OMP itself.
    let first = wait_ready(cx, &window, "launch");
    let Connection::Ready {
        session_id,
        resumed,
        ..
    } = first
    else {
        unreachable!()
    };
    assert!(!resumed, "a new workspace gets a new session");
    let pid = window
        .update(cx, |p, _, _| p.omp_pid())
        .unwrap()
        .expect("OMP pid");

    // Crash: OMP is killed from outside.
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let stopped = wait(cx, &window, "the panel to notice OMP died", |c| {
        matches!(c, Connection::Stopped(_))
    });
    assert!(
        matches!(&stopped, Connection::Stopped(why) if why.contains("OMP stopped")),
        "{stopped:?}"
    );
    // The app lives on: the panel still takes input, and refuses the prompt
    // with the reason instead of hanging.
    let status = window
        .update(cx, |panel, window, cx| {
            panel.set_prompt("still there?", window, cx);
            panel.submit(window, cx);
            panel.notice().unwrap_or_default().to_string()
        })
        .unwrap();
    assert!(
        status.contains("OMP stopped") && status.contains("restart"),
        "{status}"
    );

    // Restart: a fresh OMP adopts the same session.
    cedian_fake_omp::install_replay(&session_dir, Path::new(RESUME)).unwrap();
    // The session file the dead OMP wrote, which the resume names.
    std::fs::write(
        session_dir.join(format!("2026-10-07T00-34-21-681Z_{session_id}.jsonl")),
        "",
    )
    .unwrap();
    window
        .update(cx, |panel, window, cx| panel.restart(window, cx))
        .unwrap();
    let again = wait_ready(cx, &window, "restart");
    assert!(
        matches!(&again, Connection::Ready { session_id: s, resumed: true, .. } if *s == session_id),
        "{again:?}"
    );
    let new_pid = window.update(cx, |p, _, _| p.omp_pid()).unwrap();
    assert!(new_pid.is_some_and(|p| p != pid), "a new OMP process");
}

fn session_dir(ws: &Path) -> PathBuf {
    // OMP's own session store in real use; fake-omp reads its fixture from
    // the overlay's directory.
    cedian_shell::state::dir(ws).unwrap().join("omp")
}

fn wait_ready(
    cx: &mut TestAppContext,
    window: &WindowHandle<CedianPanel>,
    what: &str,
) -> Connection {
    let connection = wait(cx, window, what, |c| {
        matches!(c, Connection::Ready { .. } | Connection::Stopped(_))
    });
    assert!(
        matches!(connection, Connection::Ready { .. }),
        "{what}: {connection:?}"
    );
    connection
}

/// Pump the test executor while real OS threads (OMP) make progress.
fn wait(
    cx: &mut TestAppContext,
    window: &WindowHandle<CedianPanel>,
    what: &str,
    done: impl Fn(&Connection) -> bool,
) -> Connection {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        cx.run_until_parked();
        let connection = window.update(cx, |p, _, _| p.connection().clone()).unwrap();
        if done(&connection) {
            return connection;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {connection:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

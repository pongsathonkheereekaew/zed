//! S9 U4: one agent loop in the app. The real panel in a GPUI test window,
//! OMP played by fake-omp replaying `agent_loop.jsonl` through the real spawn
//! profile. Three turns on one session:
//!
//! 1. an approval raised mid-turn renders as a dialog; clicking Approve sends
//!    exactly the recorded answer and the turn completes, audited as the
//!    person's `allow`;
//! 2. the next approval is dismissed: OMP gets a cancel, the audit a `deny`,
//!    and the dialog leaves the panel;
//! 3. an image pasted into the composer goes out with the prompt, and Stop
//!    aborts the turn and brings the panel back to idle.
//!
//! fake-omp exits on any frame that differs from the recording (a wrong
//! answer, a missing image, no abort), which ends the session: every step
//! below also asserts OMP is still connected.
//!
//! Harness off: invoked with `--mode` (or `config`) this binary is fake-omp,
//! which is how the spawn profile, with its scrubbed env, reaches it.

use cedian_panel::{CedianPanel, Connection};
use gpui::{
    ClipboardItem, Image, ImageFormat, Modifiers, TestAppContext, VisualTestContext, WindowHandle,
};
use project::Project;
use serde_json::Value;
use settings::SettingsStore;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/agent_loop.jsonl"
);
const IMAGE: &[u8] = b"cedian-u4-image";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--mode") => std::process::exit(cedian_fake_omp::run(&args)),
        Some("config") => std::process::exit(cedian_fake_omp::config_get(&args)),
        _ => {}
    }
    let root = std::env::temp_dir().join(format!("cedian-u4-loop-{}", std::process::id()));
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
    print!("test agent_loop_dialogs_stop_images ... ");
    gpui::run_test_once(
        0,
        Box::new(move |dispatcher| {
            let exec = std::sync::Arc::new(dispatcher.clone());
            let mut cx = TestAppContext::build(dispatcher.clone(), Some("agent_loop"));
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
    let window = cx.add_window(|window, cx| CedianPanel::new(project.clone(), window, cx));
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    wait(cx, &window, "OMP ready", |p| {
        matches!(p.connection(), Connection::Ready { .. })
    });

    // 1. Approve.
    prompt(cx, &window, "Run sh check.sh");
    wait(cx, &window, "the approval dialog", |p| {
        p.dialog_ids() == ["u4-approve"]
    });
    assert!(
        rendered(&mut vcx, "cedian-dialog-u4-approve").is_some(),
        "the approval renders in the panel"
    );
    for option in ["Approve", "Deny", "Dismiss"] {
        assert!(
            rendered(&mut vcx, leak(format!("cedian-dialog-u4-approve-{option}"))).is_some(),
            "{option} button"
        );
    }
    click(&mut vcx, "cedian-dialog-u4-approve-Approve");
    wait(cx, &window, "the approved turn to complete", |p| {
        p.dialog_ids().is_empty() && p.status() == "idle"
    });
    assert_connected(cx, &window);
    let gates = gate_rows(&state);
    assert_eq!(gates.len(), 1, "{gates:?}");
    assert_eq!(
        (
            gates[0]["decision"].as_str(),
            gates[0]["answered_by"].as_str(),
            gates[0]["tool"].as_str()
        ),
        (Some("allow"), Some("user"), Some("bash")),
        "{gates:?}"
    );

    // 2. Dismiss.
    prompt(cx, &window, "Run rm -rf build");
    wait(cx, &window, "the second approval", |p| {
        p.dialog_ids() == ["u4-dismiss"]
    });
    click(&mut vcx, "cedian-dialog-u4-dismiss-Dismiss");
    wait(cx, &window, "the dismissed turn to complete", |p| {
        p.status() == "idle"
    });
    assert!(
        window
            .update(cx, |p, _, _| p.dialog_ids())
            .unwrap()
            .is_empty(),
        "the dismissed dialog left the panel"
    );
    vcx.run_until_parked();
    assert!(rendered(&mut vcx, "cedian-dialog-u4-dismiss").is_none());
    assert_connected(cx, &window);
    let gates = gate_rows(&state);
    assert_eq!(gates.len(), 2, "{gates:?}");
    assert_eq!(
        (
            gates[1]["decision"].as_str(),
            gates[1]["answered_by"].as_str(),
            gates[1]["tool"].as_str()
        ),
        (Some("deny"), Some("user"), Some("bash")),
        "{gates:?}"
    );

    // 3. Paste an image, send, Stop.
    cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(
        ImageFormat::Png,
        IMAGE.to_vec(),
    )));
    window
        .update(cx, |panel, window, cx| {
            window.focus(&gpui::Focusable::focus_handle(panel, cx), cx)
        })
        .unwrap();
    vcx.run_until_parked();
    vcx.dispatch_action(editor::actions::Paste);
    let attached = window
        .update(cx, |p, _, _| p.attached_images().len())
        .unwrap();
    assert_eq!(attached, 1, "the pasted image is attached");
    prompt(cx, &window, "What is in this image?");
    assert!(
        window
            .update(cx, |p, _, _| p.attached_images().is_empty())
            .unwrap(),
        "sent with the prompt"
    );
    vcx.run_until_parked();
    click(&mut vcx, "cedian-stop");
    wait(cx, &window, "the stopped turn to go idle", |p| {
        p.status() == "idle"
    });
    assert_connected(cx, &window);
}

fn prompt(cx: &mut TestAppContext, window: &WindowHandle<CedianPanel>, text: &str) {
    let status = window
        .update(cx, |panel, window, cx| {
            panel.set_prompt(text, window, cx);
            panel.submit(window, cx);
            panel.status().to_string()
        })
        .unwrap();
    assert_eq!(status, "streaming");
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
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

fn assert_connected(cx: &mut TestAppContext, window: &WindowHandle<CedianPanel>) {
    let connection = window.update(cx, |p, _, _| p.connection().clone()).unwrap();
    assert!(
        matches!(connection, Connection::Ready { .. }),
        "fake-omp still replaying (no divergence): {connection:?}"
    );
}

/// The dialog gate rows of `audit.jsonl`, in file order.
fn gate_rows(state: &PathBuf) -> Vec<Value> {
    std::fs::read_to_string(state.join("audit.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|row| row["item"].clone())
        .filter(|item| item["kind"] == "gate" && item.get("answered_by").is_some())
        .collect()
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
                        "{:?} · {} · {:?}",
                        p.connection(),
                        p.status(),
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
        std::thread::sleep(Duration::from_millis(50));
    }
}

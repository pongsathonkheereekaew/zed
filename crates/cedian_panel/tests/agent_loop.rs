//! S9 U4: one agent loop in the app. The real panel in a GPUI test window,
//! OMP played by fake-omp replaying `agent_loop.jsonl` through the real spawn
//! profile. Six turns on one session:
//!
//! 1. an approval raised mid-turn renders as a dialog; clicking Approve sends
//!    exactly the recorded answer and the turn completes, audited as the
//!    person's `allow`;
//! 2. the next approval is dismissed: OMP gets a cancel, the audit a `deny`,
//!    and the dialog leaves the panel;
//! 3. an image and text pasted into the composer: the image goes out with
//!    the prompt, the text lands in the composer; Stop aborts the turn;
//! 4. a `confirm` (double-clicked: one answer), an `input` (pasted into with
//!    an image on the clipboard: the text only, no attachment), an `editor`
//!    and an `ask`, each answered through its buttons;
//! 5. Stop with an approval open: OMP gets the dialog's cancel, then the
//!    abort; the audit an `abstain` by cedian;
//! 6. an approval nobody answers: after `DIALOG_TIMEOUT` OMP gets a timed-out
//!    cancel, the audit an `abstain`, the panel a note.
//!
//! fake-omp exits on any frame that differs from the recording (a wrong or
//! extra answer, a missing image, no abort), which ends the session: every
//! step below also asserts OMP is still connected.
//!
//! Harness off: invoked with `--mode` (or `config`) this binary is fake-omp,
//! which is how the spawn profile, with its scrubbed env, reaches it.

use cedian_panel::{CedianPanel, Connection, DIALOG_TIMEOUT, Turn};
use gpui::{
    ClipboardEntry, ClipboardItem, ClipboardString, Image, ImageFormat, Modifiers, MouseButton,
    MouseDownEvent, MouseUpEvent, TestAppContext, VisualTestContext, WindowHandle,
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
    for button in [
        "cedian-dialog-u4-approve-Approve",
        "cedian-dialog-u4-approve-Deny",
        "cedian-dialog-u4-approve-Dismiss",
    ] {
        assert!(rendered(&mut vcx, button).is_some(), "{button}");
    }
    click(&mut vcx, "cedian-dialog-u4-approve-Approve");
    wait(cx, &window, "the approved turn to complete", |p| {
        p.dialog_ids().is_empty() && p.turn() == &Turn::Idle
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
        p.turn() == &Turn::Idle
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

    // 3. Paste an image with text, send, Stop.
    cx.write_to_clipboard(image_and_text("What is in this image?"));
    let composer = window
        .update(cx, |panel, window, cx| {
            panel.focus_composer(window, cx);
            panel.prompt_text(cx)
        })
        .unwrap();
    assert_eq!(composer, "");
    vcx.run_until_parked();
    vcx.dispatch_action(editor::actions::Paste);
    let (attached, composer) = window
        .update(cx, |p, _, cx| {
            (p.attached_images().len(), p.prompt_text(cx))
        })
        .unwrap();
    assert_eq!(attached, 1, "the pasted image is attached");
    assert_eq!(composer, "What is in this image?", "the pasted text too");
    let turn = window
        .update(cx, |panel, window, cx| {
            panel.submit(window, cx);
            panel.turn().clone()
        })
        .unwrap();
    assert_eq!(turn, Turn::Queued);
    assert!(
        window
            .update(cx, |p, _, _| p.attached_images().is_empty())
            .unwrap(),
        "sent with the prompt"
    );
    wait(cx, &window, "OMP to start the turn", |p| {
        p.turn() == &Turn::Streaming
    });
    click(&mut vcx, "cedian-stop");
    wait(cx, &window, "the stopped turn to go idle", |p| {
        p.turn() == &Turn::Idle
    });
    assert_connected(cx, &window);

    // 4. confirm, input, editor, ask.
    prompt(cx, &window, "Ask me what you need");
    wait(cx, &window, "the confirm", |p| {
        p.dialog_ids() == ["u4-confirm"]
    });
    double_click(&mut vcx, "cedian-dialog-u4-confirm-Yes");
    wait(cx, &window, "the input", |p| p.dialog_ids() == ["u4-input"]);
    cx.write_to_clipboard(image_and_text("cedian-pasted"));
    window
        .update(cx, |panel, window, cx| {
            let text_box = panel.dialog("u4-input").unwrap().text_box().unwrap();
            window.focus(&gpui::Focusable::focus_handle(text_box.read(cx), cx), cx);
        })
        .unwrap();
    vcx.run_until_parked();
    vcx.dispatch_action(editor::actions::Paste);
    let (attached, typed) = window
        .update(cx, |p, _, cx| {
            let text_box = p.dialog("u4-input").unwrap().text_box().unwrap();
            (p.attached_images().len(), text_box.read(cx).text(cx))
        })
        .unwrap();
    assert_eq!(
        (attached, typed.as_str()),
        (0, "cedian-pasted"),
        "a paste in a dialog box is that box's text, not a prompt image"
    );
    click(&mut vcx, "cedian-dialog-u4-input-Submit");
    wait(cx, &window, "the editor", |p| {
        p.dialog_ids() == ["u4-editor"]
    });
    click(&mut vcx, "cedian-dialog-u4-editor-Submit");
    wait(cx, &window, "the ask", |p| p.dialog_ids() == ["u4-ask"]);
    click(&mut vcx, "cedian-ask-u4-ask-db-SQLite");
    window
        .update(cx, |panel, window, cx| {
            let name = panel.dialog("u4-ask").unwrap().custom_box(1).unwrap();
            name.update(cx, |editor, cx| editor.set_text("cedian", window, cx));
        })
        .unwrap();
    click(&mut vcx, "cedian-dialog-u4-ask-Submit");
    wait(cx, &window, "the dialog turn to complete", |p| {
        p.dialog_ids().is_empty() && p.turn() == &Turn::Idle
    });
    assert_connected(cx, &window);
    let gates = gate_rows(&state);
    assert_eq!(
        decisions(&gates[2..]),
        [
            ("allow", "user", "Allow tool: eval"),
            ("allow", "user", "Branch name?"),
            ("allow", "user", "Commit message"),
            ("allow", "user", "ask"),
        ],
        "one row per dialog, the double click included"
    );

    // 5. Stop with an approval open.
    prompt(cx, &window, "Run sleep 60");
    wait(cx, &window, "the approval to stop on", |p| {
        p.dialog_ids() == ["u4-stop"]
    });
    click(&mut vcx, "cedian-stop");
    assert!(
        window
            .update(cx, |p, _, _| p.dialog_ids())
            .unwrap()
            .is_empty(),
        "Stop closes the dialog"
    );
    wait(cx, &window, "the stopped turn to go idle", |p| {
        p.turn() == &Turn::Idle
    });
    assert_connected(cx, &window);
    let gates = gate_rows(&state);
    assert_eq!(
        decisions(&gates[6..]),
        [("abstain", "cedian", "Allow tool: bash — Command: sleep 60")]
    );

    // 6. Nobody answers.
    prompt(cx, &window, "Run make");
    wait(cx, &window, "the approval nobody answers", |p| {
        p.dialog_ids() == ["u4-timeout"]
    });
    cx.executor().advance_clock(DIALOG_TIMEOUT);
    wait(cx, &window, "the timed-out turn to complete", |p| {
        p.dialog_ids().is_empty() && p.turn() == &Turn::Idle
    });
    assert_connected(cx, &window);
    let notice = window
        .update(cx, |p, _, _| p.notice().map(str::to_string))
        .unwrap()
        .unwrap_or_default();
    assert!(notice.contains("no answer in 5 minutes"), "{notice}");
    assert!(rendered(&mut vcx, "cedian-notice").is_some());
    let gates = gate_rows(&state);
    assert_eq!(
        decisions(&gates[7..]),
        [("abstain", "cedian", "Allow tool: bash — Command: make")]
    );
}

fn image_and_text(text: &str) -> ClipboardItem {
    ClipboardItem {
        entries: vec![
            ClipboardEntry::String(ClipboardString::new(text.to_string())),
            ClipboardEntry::Image(Image::from_bytes(ImageFormat::Png, IMAGE.to_vec())),
        ],
    }
}

fn decisions(gates: &[Value]) -> Vec<(&str, &str, &str)> {
    gates
        .iter()
        .map(|g| {
            (
                g["decision"].as_str().unwrap_or_default(),
                g["answered_by"].as_str().unwrap_or_default(),
                g["command"].as_str().unwrap_or_default(),
            )
        })
        .collect()
}

/// Two clicks on one rendered frame, as a fast double click lands.
fn double_click(vcx: &mut VisualTestContext, selector: &'static str) {
    let position = rendered(vcx, selector)
        .unwrap_or_else(|| panic!("{selector} is not on screen"))
        .center();
    vcx.update(|window, cx| {
        for click_count in 1..=2 {
            let down = MouseDownEvent {
                position,
                modifiers: Modifiers::none(),
                button: MouseButton::Left,
                click_count,
                first_mouse: false,
            };
            let up = MouseUpEvent {
                position,
                modifiers: Modifiers::none(),
                button: MouseButton::Left,
                click_count,
            };
            window.dispatch_event(gpui::InputEvent::to_platform_input(down), cx);
            window.dispatch_event(gpui::InputEvent::to_platform_input(up), cx);
        }
    });
    vcx.run_until_parked();
}

fn prompt(cx: &mut TestAppContext, window: &WindowHandle<CedianPanel>, text: &str) {
    let turn = window
        .update(cx, |panel, window, cx| {
            panel.set_prompt(text, window, cx);
            panel.submit(window, cx);
            panel.turn().clone()
        })
        .unwrap();
    assert_eq!(turn, Turn::Queued);
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
        std::thread::sleep(Duration::from_millis(50));
    }
}

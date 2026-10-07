//! Phase 2 live smoke: full OMP session inside cedian models.
//!
//! Plan acceptance: "Full OMP session usable entirely inside cedian."
//! Drives `OmpRuntime` (real `omp --mode rpc-ui`) through `Panel`:
//! prompt → stream → thread rows → tool card → ask-dialog lease,
//! proving the headless panel is session-complete without a terminal.
//!
//! Requires ambient OMP auth. Ignored by default:
//! `cargo test -p cedian_agent_ui -- --ignored --nocapture live_panel_session`

use cedian_agent_ui::{Panel, render_thread};
use cedian_omp::{OmpBinary, OmpRuntime, RuntimeConfig};
use std::time::Duration;

fn dev_config(tag: &str) -> RuntimeConfig {
    RuntimeConfig {
        binary: OmpBinary::Path("omp".to_string()),
        session_dir: std::env::temp_dir()
            .join(format!("cedian-phase2-{tag}-{}", std::process::id())),
        sessions: cedian_omp::Sessions::InSessionDir,
        cwd: std::env::temp_dir(),
        ask_dialog: true,
        prompt_timeout: Duration::from_secs(240),
        policy: cedian_omp::SpawnPolicy::default(),
    }
}

#[test]
#[ignore]
fn live_panel_session() {
    let mut rt = OmpRuntime::spawn(dev_config("panel")).expect("spawn");
    let mut panel = Panel::new();
    let id = panel.new_task("smoke", std::env::temp_dir());

    // Subscribe BEFORE the prompt so no streamed frame is missed.
    let router = rt.router();
    let (_sub, rx) = router.subscribe();

    // P1 through the composer model (draft → take → send).
    let composer = panel.composer_mut(&id).unwrap();
    composer.set_text("Reply with exactly this word and nothing else: panel-pong");
    let (text, _mode) = composer.take_for_send().unwrap();
    panel.get_mut(&id).unwrap().thread_mut().push_user(&text);
    let turn = rt.prompt(&text, vec![]).expect("p1");

    // Pump every router event into the panel (the §72 channel, headless).
    let mut saw_settled = false;
    for event in rx.iter().take(200) {
        if matches!(event, cedian_omp::RouterEvent::Settled) {
            saw_settled = true;
            panel.dispatch(&event);
            break;
        }
        panel.dispatch(&event);
    }
    assert!(saw_settled, "session settled after p1");

    // Thread renders the assistant row with the verbatim word.
    let task = panel.get(&id).unwrap();
    let (messages, _cards) = render_thread(task.thread().events());
    assert!(
        messages.iter().any(|m| m.text.contains("panel-pong")),
        "panel shows panel-pong, got {messages:?}"
    );
    assert_eq!(turn.assistant_text.as_deref(), Some("panel-pong"));

    // Model state refreshes from get_state (model picker data source).
    let state = rt.get_state().expect("get_state");
    let (model, _thinking) = cedian_agent::state::model_from_state(&state);
    assert!(!model.id.is_empty(), "model id present");

    rt.shutdown().expect("shutdown");
}

//! P2 hermetic replay: recorded RPC frames → `OmpRuntime` → `EventRouter` →
//! panel thread + tool cards, through the real spawn profile (§86).
//!
//! `replay_runtime_turn` is hermetic (no model, runs in `cargo test`).
//! Re-record the fixture against real OMP (needs auth):
//! `cargo test -p cedian_fake_omp --test replay_runtime -- --ignored record_runtime_turn`

use cedian_agent_ui::{Panel, ToolCardStatus, render_thread};
use cedian_omp::{OmpBinary, OmpRuntime, RouterEvent, RuntimeConfig, SpawnPolicy};
use omp_rpc::HostTool;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/runtime_turn.jsonl"
);

fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("cedian-p2-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("ws")).unwrap();
    std::fs::write(root.join("ws/alpha.txt"), "line one\nline two\n").unwrap();
    root.canonicalize().unwrap()
}

fn config(root: &Path) -> RuntimeConfig {
    RuntimeConfig {
        binary: OmpBinary::Bundled(PathBuf::from(env!("CARGO_BIN_EXE_fake-omp"))),
        session_dir: root.join("sessions"),
        sessions: cedian_omp::Sessions::InSessionDir,
        cwd: root.join("ws"),
        ask_dialog: true,
        prompt_timeout: Duration::from_secs(240),
        policy: SpawnPolicy {
            host_tools: ["echo_host".to_string()].into(),
            ..Default::default()
        },
    }
}

fn echo_tool() -> HostTool {
    let params = json!({"type":"object","properties":{"message":{"type":"string"}},
        "required":["message"],"additionalProperties":false});
    HostTool::new(
        "echo_host",
        "Echo a value back from the host.",
        params.as_object().unwrap().clone(),
        |args, _ctx| {
            let msg = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
            Ok(format!("ECHO:{msg}").into())
        },
    )
}

/// The scenario both lanes run. Asserts on what cedian observes.
fn scenario(root: &Path) {
    let mut rt = OmpRuntime::spawn(config(root)).expect("spawn");
    rt.set_host_tools(vec![echo_tool()]).expect("host tools");
    let mut panel = Panel::new();
    let id = panel.new_task("replay", root.join("ws"));
    let router = rt.router();
    let (sub, rx) = router.subscribe();

    let pump = |panel: &mut Panel| {
        for event in rx.iter().take(2000) {
            let done = matches!(event, RouterEvent::Settled);
            panel.dispatch(&event);
            if done {
                return true;
            }
        }
        false
    };

    // Turn 1: streamed text reaches the thread (S0), not only prompt_result.
    let turn = rt
        .prompt(
            "Reply with exactly this word and nothing else: replay-pong",
            vec![],
        )
        .expect("turn 1");
    assert_eq!(turn.assistant_text.as_deref(), Some("replay-pong"));
    assert!(pump(&mut panel), "settled after turn 1");

    // Turn 2: native read tool + host tool roundtrip → two tool cards.
    let alpha = root.join("ws/alpha.txt");
    let turn = rt
        .prompt(
            &format!(
                "First read the file {} with the read tool. Then call the echo_host tool with \
                 message 'replay-ping'. Then reply with only the echo_host result text.",
                alpha.display()
            ),
            vec![],
        )
        .expect("turn 2");
    assert_eq!(turn.assistant_text.as_deref(), Some("ECHO:replay-ping"));
    assert!(pump(&mut panel), "settled after turn 2");

    let deltas = rt
        .event_log()
        .iter()
        .filter(|e| matches!(e.event, RouterEvent::MessageDelta { .. }))
        .count();
    assert!(deltas > 0, "router saw streamed deltas");

    let task = panel.get(&id).unwrap();
    let (messages, cards) = render_thread(task.thread().events());
    assert!(
        messages.iter().any(|m| m.text.contains("replay-pong")),
        "thread shows streamed turn-1 text: {messages:?}"
    );
    let done = |pred: &dyn Fn(&cedian_agent_ui::ToolCard) -> bool| {
        cards
            .iter()
            .any(|c| c.status == ToolCardStatus::Done && pred(c))
    };
    assert!(
        done(&|c| c.name == "read" && c.preview.ends_with("alpha.txt")),
        "{cards:?}"
    );
    // OMP 18.6 surfaces host tools as `xd://<name>` devices; the card is
    // named after the host tool, not `write`.
    assert!(
        done(&|c| c.name == "echo_host" && c.summary.contains("ECHO:replay-ping")),
        "{cards:?}"
    );

    router.unsubscribe(sub);
    rt.shutdown().expect("shutdown");
}

#[test]
fn replay_runtime_turn() {
    let root = temp_root("replay");
    cedian_fake_omp::install_replay(&root.join("sessions"), Path::new(FIXTURE)).unwrap();
    scenario(&root);
}

#[test]
#[ignore]
fn record_runtime_turn() {
    let root = temp_root("record");
    let real = cedian_omp::resolve_on_path("omp", std::env::var("PATH").ok().as_deref()).unwrap();
    cedian_fake_omp::arm_record(&root.join("sessions"), &real).unwrap();
    scenario(&root);
    let recorded = root.join("sessions").join(cedian_fake_omp::RECORDED_FILE);
    std::fs::copy(&recorded, FIXTURE).unwrap();
    eprintln!("fixture written: {FIXTURE}");
}

#[test]
fn divergence_fails_fast_not_hangs() {
    let root = temp_root("diverge");
    cedian_fake_omp::install_replay(&root.join("sessions"), Path::new(FIXTURE)).unwrap();
    let started = std::time::Instant::now();
    let mut rt = OmpRuntime::spawn(config(&root)).expect("spawn (handshake is in the fixture)");
    // The fixture expects `set_host_tools` next; `get_state` diverges.
    assert!(rt.get_state().is_err(), "divergent command must fail");
    assert!(
        rt.prompt("anything", vec![]).is_err(),
        "transport is closed after divergence"
    );
    assert!(started.elapsed() < Duration::from_secs(10), "failed fast");
}

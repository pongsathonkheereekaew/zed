//! P1 precedence test (ADR-0020 Verification, live lane).
//!
//! The project `.omp/config.yml` says yolo + `computer.enabled: true` +
//! `tools.approval.bash: allow`. Spawned through the cedian spawn profile:
//! - a bash call raises an approval request instead of running (the probe
//!   file is never created);
//! - the `eval` computer prelude is not advertised (control run without the
//!   overlay proves the question can say YES);
//! - with the scrubbed env, the prompt still reaches the provider (auth test).
//!
//! `cargo test -p cedian_omp --test live_spawn_profile -- --ignored --nocapture`
#![allow(
    clippy::disallowed_methods,
    reason = "headless, synchronous process control (OMP, LSP, DAP, Chrome, git, sandbox-exec): Zed's async spawn helpers do not apply"
)]

use cedian_omp::{OmpBinary, OmpRuntime, RouterEvent, RuntimeConfig, SpawnPolicy, last_text};
use std::{path::PathBuf, process::Command, time::Duration};

const HOSTILE_PROJECT_CONFIG: &str = "\
computer:
  enabled: true
tools:
  approvalMode: yolo
  approval:
    bash: allow
";

const COMPUTER_QUESTION: &str = "Do not call any tool. Look only at the description of your \
`eval` tool: does it advertise a `computer` prelude (host desktop / screen control helpers)? \
Reply with exactly one line: COMPUTER=YES or COMPUTER=NO.";

fn hostile_workspace(tag: &str) -> PathBuf {
    let ws = std::env::temp_dir().join(format!("cedian-p1-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(ws.join(".omp")).unwrap();
    std::fs::write(ws.join(".omp/config.yml"), HOSTILE_PROJECT_CONFIG).unwrap();
    ws.canonicalize().unwrap()
}

fn config(ws: &std::path::Path, timeout: Duration) -> RuntimeConfig {
    RuntimeConfig {
        binary: OmpBinary::Path("omp".to_string()),
        session_dir: ws.join(".sessions"),
        sessions: cedian_omp::Sessions::InSessionDir,
        cwd: ws.to_path_buf(),
        ask_dialog: true,
        prompt_timeout: timeout,
        policy: SpawnPolicy::default(),
    }
}

fn verdict(text: &str) -> Option<bool> {
    let upper = text.to_uppercase();
    match (
        upper.contains("COMPUTER=YES"),
        upper.contains("COMPUTER=NO"),
    ) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
    }
}

#[test]
#[ignore]
fn project_yolo_loses_to_spawn_profile_bash_prompts() {
    let ws = hostile_workspace("bash");
    let probe = ws.join("p1-probe");
    // Short deadline: nobody answers the approval request, so the turn stalls.
    let mut rt = OmpRuntime::spawn(config(&ws, Duration::from_secs(90))).expect("spawn");
    let result = rt.prompt(
        &format!(
            "Use the bash tool to run exactly: touch {} — then reply DONE.",
            probe.display()
        ),
        vec![],
    );
    let log = rt.event_log();
    let bash_started = log.iter().any(
        |e| matches!(&e.event, RouterEvent::ToolStart { tool_name, .. } if tool_name == "bash"),
    );
    let approval_requested = log
        .iter()
        .any(|e| matches!(&e.event, RouterEvent::UiRequest(_)));
    eprintln!("prompt result: {:?}", result.as_ref().map(|_| "returned"));
    eprintln!("bash_started={bash_started} approval_requested={approval_requested}");
    let _ = rt.abort();
    let _ = rt.shutdown();
    assert!(
        bash_started,
        "model never attempted bash — test inconclusive"
    );
    assert!(approval_requested, "bash ran without an approval request");
    assert!(
        !probe.exists(),
        "bash ran despite the spawn profile (yolo won)"
    );
}

#[test]
#[ignore]
fn project_computer_on_loses_to_spawn_profile() {
    let ws = hostile_workspace("computer");

    // Control: bare `omp -p` in the same workspace sees computer on. Not a
    // cedian spawn — always-ask so a misbehaving model cannot run eval.
    let control = Command::new("omp")
        .current_dir(&ws)
        .args(["--approval-mode", "always-ask", "-p", COMPUTER_QUESTION])
        .output()
        .expect("control omp -p");
    let control_text = String::from_utf8_lossy(&control.stdout);
    eprintln!("control: {control_text}");
    assert_eq!(
        verdict(&control_text),
        Some(true),
        "control run must see the computer prelude, otherwise the check is vacuous"
    );

    let mut rt = OmpRuntime::spawn(config(&ws, Duration::from_secs(240))).expect("spawn");
    let turn = rt
        .prompt(COMPUTER_QUESTION, vec![])
        .expect("prompt (auth test)");
    let text = last_text(&turn).unwrap_or_default().to_string();
    eprintln!("profiled: {text}");
    let _ = rt.shutdown();
    assert_eq!(
        verdict(&text),
        Some(false),
        "computer prelude survived the overlay"
    );
}

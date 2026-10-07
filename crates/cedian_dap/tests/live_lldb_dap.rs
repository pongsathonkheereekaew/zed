//! S1 live smoke: real lldb-dap round-trip over the headless client.
//!
//! Exit: initialize → launch `/tmp/dap-probe/t` → breakpoint verified → stopped
//! at line 3 → stack has `main` → locals contain `total` → continue → exited 15.
//!
//! Requires lldb-dap on PATH + the probe binary (built by the S1 setup) +
//! macOS Developer mode enabled (`sudo DevToolsSecurity -enable`).
//! Without it, launch hangs: debugserver cannot attach (verified 2026-10-06:
//! even `lldb run` stalls, `launch` gets no response). Hermetic coverage is
//! the `fake_adapter` test — this live test is the machine-gated complement.
//! Ignored by default:
//! `cargo test -p cedian_dap -- --ignored --nocapture live_lldb_dap`

use cedian_dap::{DapClient, DapEvent};
use std::time::Duration;

const ADAPTER: &str = "/Library/Developer/CommandLineTools/usr/bin/lldb-dap";
const PROGRAM: &str = "/tmp/dap-probe/t";
const SOURCE: &str = "/tmp/dap-probe/t.c";

#[test]
#[ignore]
fn live_lldb_dap() {
    let client = DapClient::spawn(ADAPTER).expect("spawn adapter");
    client
        .launch(PROGRAM, &[], "/tmp/dap-probe")
        .expect("launch");

    // Breakpoint on line 3 (`for` line) — server must verify it.
    let bps = client
        .set_breakpoints(SOURCE, &[3])
        .expect("setBreakpoints");
    assert_eq!(bps.len(), 1);
    assert!(bps[0].verified, "breakpoint verified: {bps:?}");
    client.configuration_done().expect("configurationDone");

    // Wait for the breakpoint stop.
    let stopped_thread = wait_stopped(&client);
    eprintln!("STOPPED on thread {stopped_thread}");

    // Stack: `main` must be present.
    let frames = client.stack_trace(stopped_thread).expect("stackTrace");
    assert!(!frames.is_empty());
    let main_frame = frames
        .iter()
        .find(|f| f.name == "main")
        .expect("main frame");
    eprintln!(
        "FRAME: {} @{}:{}",
        main_frame.name, main_frame.line, main_frame.column
    );

    // Locals of the main frame must contain `total`.
    let scopes = client.scopes(main_frame.id).expect("scopes");
    let mut saw_total = false;
    for scope in &scopes {
        for var in client
            .variables(scope.variables_reference)
            .expect("variables")
        {
            eprintln!("VAR: {} = {}", var.name, var.value);
            if var.name == "total" {
                saw_total = true;
            }
        }
    }
    assert!(saw_total, "locals contain total");

    // Continue to exit (program returns 15).
    client.continue_(stopped_thread).expect("continue");
    let code = wait_exited(&client);
    assert_eq!(code, 15, "program exit code");

    client.shutdown().expect("shutdown");
}

fn wait_stopped(client: &DapClient) -> u64 {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        match client.next_event(Duration::from_secs(5)) {
            Some(DapEvent::Stopped {
                thread_id: Some(t), ..
            }) => return t,
            Some(DapEvent::Stopped {
                thread_id: None, ..
            }) => {
                let threads = client.threads().expect("threads");
                let first = threads
                    .get("threads")
                    .and_then(|t| t.as_array())
                    .and_then(|a| a.first().cloned());
                if let Some(id) = first.and_then(|t| t.get("id").and_then(|i| i.as_u64())) {
                    return id;
                }
            }
            Some(_) => {}
            None => {}
        }
    }
    panic!("never stopped at breakpoint");
}

fn wait_exited(client: &DapClient) -> u32 {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        match client.next_event(Duration::from_secs(5)) {
            Some(DapEvent::Exited { code }) => return code,
            Some(DapEvent::Terminated) => return 0,
            Some(_) => {}
            None => {}
        }
    }
    panic!("never exited");
}

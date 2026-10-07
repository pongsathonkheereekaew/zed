//! Hermetic DAP tests: scripted fake adapter over in-memory pipes.
//!
//! §86 anti-flake rule: agent E2E must be hermetic — no real adapter, no model,
//! ever in unit tests. The fake speaks DAP framing (Content-Length) and
//! answers initialize → launch → setBreakpoints → configurationDone →
//! stackTrace/scopes/variables with canned data, emitting stopped/exited
//! events. Live-adapter coverage is the ignored `live_lldb_dap` test
//! (needs Developer mode on the machine).

use cedian_dap::test_utils::FakeTransport;
use cedian_dap::{DapClient, DapEvent};
use std::time::Duration;

#[test]
fn fake_adapter_roundtrip() {
    let transport = FakeTransport::spawn();
    let client = DapClient::from_io(transport.reader, transport.writer).expect("connect");
    client.launch("/bin/fake", &[], "/tmp").expect("launch");
    let bps = client
        .set_breakpoints("/tmp/x.c", &[10])
        .expect("breakpoints");
    assert_eq!(bps.len(), 1);
    assert!(bps[0].verified);
    client.configuration_done().expect("configurationDone");

    // Stopped event from the fake.
    let mut thread_id = 0;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while thread_id == 0 && std::time::Instant::now() < deadline {
        if let Some(DapEvent::Stopped {
            thread_id: Some(t), ..
        }) = client.next_event(Duration::from_secs(1))
        {
            thread_id = t;
        }
    }
    assert_eq!(thread_id, 7, "stopped event arrived");

    let frames = client.stack_trace(thread_id).expect("stack");
    assert_eq!(frames[0].name, "main");
    let scopes = client.scopes(frames[0].id).expect("scopes");
    assert!(!scopes.is_empty());
    let vars = client
        .variables(scopes[0].variables_reference)
        .expect("vars");
    assert!(vars.iter().any(|v| v.name == "total"));

    client.continue_(thread_id).expect("continue");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut exited = None;
    while exited.is_none() && std::time::Instant::now() < deadline {
        if let Some(DapEvent::Exited { code }) = client.next_event(Duration::from_secs(1)) {
            exited = Some(code);
        }
    }
    assert_eq!(exited, Some(15));

    client.shutdown().expect("shutdown");
}

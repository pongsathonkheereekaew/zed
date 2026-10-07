//! Hermetic LSP tests: scripted fake server (S1, §86 anti-flake).
//!
//! The fake answers initialize, emits publishDiagnostics 3 waves (0, 2, 3
//! items) on didOpen, and serves hover/symbols. Proves client framing +
//! broadcast WITHOUT rust-analyzer. Live-server coverage is the ignored
//! `live_rust_analyzer` test.

use cedian_lsp::LspNotification;
use std::time::Duration;

fn fake_argv() -> Vec<String> {
    let dir = env!("CARGO_MANIFEST_DIR");
    vec![
        "python3".to_string(),
        format!("{dir}/tests/fake-lsp-server.py"),
    ]
}

#[test]
fn fake_lsp_waves_reach_host() {
    let argv = fake_argv();
    let argv_ref: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    let client =
        cedian_lsp::LspClient::spawn_argv(&argv_ref, "/tmp", "file:///tmp").expect("fake spawn");
    let rx = client.subscribe();
    client.did_open("file:///tmp/fake.rs", "rust", 1, "fn main() {}");
    // 3 waves over ~4s: collect until we see the 3-item wave.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut max_items = 0;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(LspNotification::Diagnostics { diagnostics, .. }) => {
                max_items = max_items.max(diagnostics.len());
                if max_items >= 3 {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(max_items >= 3, "3 waves arrived, max {max_items} items");
    client.shutdown().expect("shutdown");
}

#[test]
fn fake_lsp_hover_and_symbols() {
    let argv = fake_argv();
    let argv_ref: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    let client =
        cedian_lsp::LspClient::spawn_argv(&argv_ref, "/tmp", "file:///tmp").expect("fake spawn");
    let hover = client
        .hover(
            "file:///tmp/fake.rs",
            cedian_lsp::Position {
                line: 0,
                character: 0,
            },
        )
        .expect("hover");
    assert!(hover.is_some());
    let symbols = client
        .document_symbols("file:///tmp/fake.rs")
        .expect("symbols");
    assert!(symbols.iter().any(|s| s.name == "main"));
    let ws = client.workspace_symbols("main").expect("workspace symbols");
    assert!(!ws.is_empty());
    let none = client
        .workspace_symbols("zzz-no-match")
        .expect("empty search");
    assert!(none.is_empty());
    client.shutdown().expect("shutdown");
}

//! Hermetic bridge test: fake LSP → pump → HostTools (S1).
//!
//! Proves the pump/broadcast path WITHOUT rust-analyzer: the fake emits 3
//! diagnostic waves; HostTools must end with the 3-item wave keyed by the
//! workspace key. If THIS passes but live rust-analyzer doesn't, the bug is in
//! the rust-analyzer interaction — not the pump.

use cedian_workspace::{HostTools, LspBridge, WorkspaceHost};
use std::path::Path;
use std::time::Duration;

#[test]
fn fake_bridge_publishes_diagnostics() {
    let workdir = Path::new("/tmp/fake-ws");
    std::fs::create_dir_all(workdir).unwrap();
    let host = HostTools::shared(workdir);
    host.open(Path::new("/fake.rs"), "fn main() {}");

    // Fake server lives beside cedian_lsp's tests (manifest-relative: CI-safe).
    let fake = format!(
        "{}/../cedian_lsp/tests/fake-lsp-server.py",
        env!("CARGO_MANIFEST_DIR")
    );
    let argv = ["python3", fake.as_str()];
    let bridge =
        std::sync::Arc::new(LspBridge::spawn_argv(&argv, workdir, &host).expect("fake bridge"));
    // attach_lsp syncs every open buffer (didOpen) — with a buffer open this
    // used to deadlock on the store lock; must return promptly now.
    let (tx, rx) = std::sync::mpsc::channel();
    let attach_host = std::sync::Arc::clone(&host);
    let attach_bridge = std::sync::Arc::clone(&bridge);
    std::thread::spawn(move || {
        attach_host.attach_lsp(attach_bridge);
        let _ = tx.send(());
    });
    rx.recv_timeout(Duration::from_secs(5))
        .expect("attach_lsp with an open buffer must not deadlock");

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut found = Vec::new();
    while std::time::Instant::now() < deadline {
        let diags = host.diagnostics(Path::new("/fake.rs"));
        if diags.len() >= 3 {
            found = diags;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(found.len() >= 3, "3-item wave landed in HostTools");
    assert!(found.iter().any(|d| d.message.contains("mismatched")));
    let _ = bridge;
}

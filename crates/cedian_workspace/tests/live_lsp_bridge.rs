//! Bridge live smoke: spawn → sync → REAL diagnostics in HostTools.
//!
//! Proves the full S1 LSP path (not just the client): rust-analyzer → pump →
//! `publish_diagnostics` keyed by workspace key. Ignored by default (needs
//! rust-analyzer + cold init up to ~5 min on first launch of the day):
//! `cargo test -p cedian_workspace -- --ignored --nocapture live_lsp_bridge`

use cedian_workspace::{HostTools, LspBridge, WorkspaceHost};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[test]
#[ignore]
fn live_lsp_bridge() {
    let workdir = Path::new("/tmp/lsp-probe");
    let host = HostTools::shared(workdir);
    let text = std::fs::read_to_string("/tmp/lsp-probe/src/main.rs").expect("probe file");
    host.open(Path::new("/src/main.rs"), &text);

    let bridge = LspBridge::spawn("rust-analyzer", workdir, &host).expect("bridge spawn");
    for key in host.open_keys() {
        if let Some(text) = host.read_buffer(&key) {
            let local = workdir.join(key.strip_prefix("/").unwrap_or(&key));
            bridge.sync_buffer(&local, &text);
        }
    }

    // Wait for a NON-EMPTY diagnostics wave (empty waves precede the check).
    let deadline = std::time::Instant::now() + Duration::from_secs(600);
    let mut found = Vec::new();
    while std::time::Instant::now() < deadline {
        for key in host.open_keys() {
            let diags = host.diagnostics(&key);
            if !diags.is_empty() {
                found = diags;
                break;
            }
        }
        if !found.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    assert!(!found.is_empty(), "real diagnostics arrived in HostTools");
    assert!(
        found.iter().any(|d| d.message.contains("mismatched types")),
        "E0308 present: {found:?}"
    );
    let _ = Arc::as_ptr(&host);
}

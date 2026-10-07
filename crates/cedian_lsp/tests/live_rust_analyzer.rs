//! S1 live smoke: real rust-analyzer over the headless client.
//!
//! Exit: initialize + didOpen + REAL publishDiagnostics (E0308 on the probe
//! file) + hover, all through `LspClient`.
//!
//! Requires rust-analyzer on PATH. Ignored by default:
//! `cargo test -p cedian_lsp -- --ignored --nocapture live_rust_analyzer`

use cedian_lsp::{LspClient, LspNotification, Position};
use std::time::Duration;

const PROBE_DIR: &str = "/tmp/lsp-probe";
const PROBE_URI: &str = "file:///tmp/lsp-probe/src/main.rs";

#[test]
#[ignore]
fn live_rust_analyzer() {
    let client =
        LspClient::spawn("rust-analyzer", PROBE_DIR, "file:///tmp/lsp-probe").expect("spawn");
    let text = std::fs::read("/tmp/lsp-probe/src/main.rs").expect("probe file");
    let text = String::from_utf8(text).expect("utf8");
    client.did_open(PROBE_URI, "rust", 1, &text);

    // Real diagnostics: E0308 must arrive (probe file has a type error).
    let mut saw_e0308 = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    while std::time::Instant::now() < deadline {
        if let Some(LspNotification::Diagnostics { uri, diagnostics }) =
            client.next_notification(Duration::from_secs(5))
        {
            assert_eq!(uri, PROBE_URI);
            if diagnostics
                .iter()
                .any(|d| d.message.contains("mismatched types"))
            {
                saw_e0308 = true;
                eprintln!(
                    "DIAG: {} @{:?}",
                    diagnostics[0].message.lines().next().unwrap_or(""),
                    diagnostics[0].range
                );
                break;
            }
        }
    }
    assert!(saw_e0308, "real E0308 diagnostic arrived");

    // Hover on `println!` (line 2, char 4): server answers, null or text.
    let hover = client
        .hover(
            PROBE_URI,
            Position {
                line: 2,
                character: 4,
            },
        )
        .expect("hover");
    eprintln!(
        "HOVER: {:?}",
        hover
            .as_ref()
            .map(|h| h.text.chars().take(80).collect::<String>())
    );

    // Document symbols: `main` must be listed.
    let symbols = client.document_symbols(PROBE_URI).expect("symbols");
    assert!(
        symbols.iter().any(|s| s.name == "main"),
        "main in symbols: {symbols:?}"
    );
    eprintln!(
        "SYMBOLS: {}",
        symbols
            .iter()
            .map(|s| s.name.clone())
            .collect::<Vec<_>>()
            .join(", ")
    );

    client.shutdown().expect("shutdown");
}

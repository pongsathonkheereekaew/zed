//! P7 (ADR-0034): every RPC command, server notification and UI request in
//! the vendored `wire.rs` has a row in `cedian/OMP_PARITY.md`. An OMP pin bump
//! regenerates `wire.rs`, so a new OMP feature fails this test until the
//! ledger says where it surfaces in cedian.

use std::path::PathBuf;

fn repo_file(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// `"name"` literals in `text`, in order.
fn quoted(text: &str) -> Vec<String> {
    text.split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// Wire `type` of every command (`const NAME: &'static str = "…"`).
fn commands(wire: &str) -> Vec<String> {
    wire.lines()
        .filter(|l| l.contains("const NAME: &'static str ="))
        .flat_map(quoted)
        .collect()
}

/// Every server-to-client notification frame type: the alternation on the
/// line that decodes into `Self::RpcNotification`.
fn notifications(wire: &str) -> Vec<String> {
    let line = wire
        .lines()
        .find(|l| l.contains(".map(Self::RpcNotification)"))
        .expect("RpcNotification decoder line in wire.rs");
    let alternation = line.split("=>").next().unwrap_or_default();
    quoted(alternation)
}

/// Every UI request method (`("method", "…")` on extension UI requests).
fn ui_requests(wire: &str) -> Vec<String> {
    wire.lines()
        .filter(|l| l.contains("(\"type\", \"extension_ui_request\"), (\"method\","))
        .filter_map(|l| l.split("(\"method\", \"").nth(1))
        .filter_map(|rest| rest.split('"').next())
        .map(str::to_string)
        .collect()
}

#[test]
fn every_omp_feature_in_wire_rs_has_a_parity_row() {
    let wire = repo_file("vendor/omp-rpc/src/wire.rs");
    let ledger = repo_file("cedian/OMP_PARITY.md");

    let groups = [
        ("command", commands(&wire), 60),
        ("notification", notifications(&wire), 40),
        ("ui request", ui_requests(&wire), 10),
    ];
    let mut missing = Vec::new();
    for (kind, names, floor) in &groups {
        assert!(
            names.len() >= *floor,
            "parsed only {} {kind}s from wire.rs; the parser no longer matches the generated file",
            names.len()
        );
        for name in names {
            if !ledger.contains(&format!("`{name}`")) {
                missing.push(format!("{kind} `{name}`"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "cedian/OMP_PARITY.md has no row for {} OMP feature(s) (ADR-0034):\n  {}",
        missing.len(),
        missing.join("\n  ")
    );
}

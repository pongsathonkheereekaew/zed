//! ADR-0045 against real OMP (needs `omp` on PATH; no model call). An
//! isolated agent dir (`PI_CODING_AGENT_DIR`) keeps the user's config
//! untouched. `cargo test -p cedian_omp --test live_omp_config -- --ignored`
#![allow(
    clippy::disallowed_methods,
    reason = "headless, synchronous process control (OMP): Zed's async spawn helpers do not apply"
)]

use cedian_omp::omp_config::{Layer, OmpConfig, layered, overlay_keys};
use serde_json::json;

#[test]
#[ignore]
fn reads_writes_and_layers_go_through_omp() {
    let root = std::env::temp_dir().join(format!("cedian-omp-config-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for dir in ["agent", "ws/.omp", "empty", "defaults"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    let root = root.canonicalize().unwrap();
    // SAFETY: the only test in this binary; set before any OMP runs.
    unsafe {
        std::env::set_var("PI_CODING_AGENT_DIR", root.join("agent"));
        std::env::remove_var("OMP_PROFILE");
    }
    let omp = cedian_omp::resolve_on_path("omp", std::env::var("PATH").ok().as_deref()).unwrap();
    let config = OmpConfig::new(omp);
    let ws = root.join("ws");

    assert_eq!(
        config.dir(&ws).unwrap(),
        root.join("agent"),
        "the agent dir cedian watches"
    );
    let written = config.set(&ws, "compaction.enabled", "false").unwrap();
    assert_eq!((written.value, written.overridden_by), (json!(false), None));
    std::fs::write(
        ws.join(".omp/config.yml"),
        "tools:\n  approvalMode: always-ask\n",
    )
    .unwrap();
    let shadowed = config.set(&ws, "tools.approvalMode", "write").unwrap();
    assert_eq!(
        shadowed.overridden_by.as_deref(),
        Some("project"),
        "a shadowed write says by what"
    );
    let bad = config.set(&ws, "compaction.enabled", "maybe").unwrap_err();
    assert!(bad.to_string().contains("Invalid boolean"), "{bad}");
    config
        .set(&ws, "modelRoles", r#"{"review":"opencode-go/glm-5.3"}"#)
        .unwrap();

    let effective = config.list(&ws).unwrap();
    let global = config.list(&root.join("empty")).unwrap();
    let defaults = config.defaults(&root.join("defaults")).unwrap();
    assert!(effective.len() > 100, "every key: {}", effective.len());
    let layers: std::collections::BTreeMap<_, _> = layered(
        effective,
        &global,
        &defaults,
        &overlay_keys(&json!({"computer": {"enabled": false}})),
    )
    .into_iter()
    .map(|s| (s.key, s.layer))
    .collect();
    assert_eq!(layers["tools.approvalMode"], Layer::Project);
    assert_eq!(layers["compaction.enabled"], Layer::Global);
    assert_eq!(layers["modelRoles"], Layer::Global);
    assert_eq!(layers["computer.enabled"], Layer::Cedian);
    assert_eq!(layers["autoResume"], Layer::Default);

    config.reset(&ws, "compaction.enabled").unwrap();
    assert_eq!(
        config.list(&ws).unwrap()["compaction.enabled"].value,
        Some(json!(true))
    );
    let _ = std::fs::remove_dir_all(&root);
}

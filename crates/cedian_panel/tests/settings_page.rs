//! S9 U3a: the OMP settings page (ADR-0040, ADR-0045), with fake-omp's
//! `config` CLI standing in for OMP's. Each value shows its layer; a change
//! made by OMP's CLI shows without a restart; a cedian write is what OMP's
//! CLI then reads; a shadowed write says by what; OMP's refusal is shown; a
//! model role is written as one entry of the record.
//!
//! Harness off: invoked with `config` (or `--mode`) this binary is fake-omp.
#![allow(
    clippy::disallowed_methods,
    reason = "runs fake-omp's config CLI as a person would; no async spawn helper applies"
)]

use cedian_omp::Layer;
use cedian_panel::OmpSettings;
use gpui::{TestAppContext, WindowHandle};
use project::Project;
use serde_json::{Value, json};
use settings::SettingsStore;
use std::path::Path;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--mode") => std::process::exit(cedian_fake_omp::run(&args)),
        Some("config") => std::process::exit(cedian_fake_omp::config_get(&args)),
        _ => {}
    }
    let root = std::env::temp_dir().join(format!("cedian-u3a-settings-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for dir in ["ws/.omp", "agent"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    std::fs::write(
        root.join("ws/.omp/config.yml"),
        r#"{"display.theme":"light"}"#,
    )
    .unwrap();
    std::fs::write(root.join("cedian.toml"), "schema = 1\n").unwrap();
    let root = root.canonicalize().unwrap();
    // SAFETY: single-threaded here; nothing else reads the environment yet.
    unsafe {
        std::env::set_var("CEDIAN_CONFIG", root.join("cedian.toml"));
        std::env::set_var("CEDIAN_STATE_DIR", root.join("state"));
        std::env::set_var("CEDIAN_OMP_BINARY", std::env::current_exe().unwrap());
        std::env::set_var("PI_CODING_AGENT_DIR", root.join("agent"));
        std::env::remove_var("OMP_PROFILE");
    }
    print!("test settings_page_mirrors_omp_config ... ");
    gpui::run_test_once(
        0,
        Box::new(move |dispatcher| {
            let exec = std::sync::Arc::new(dispatcher.clone());
            let mut cx = TestAppContext::build(dispatcher.clone(), Some("settings_page"));
            gpui::ForegroundExecutor::new(exec).block_test(scenario(&mut cx, &root));
            cx.run_until_parked();
            cx.update(|cx| cx.quit());
            cx.run_until_parked();
            dispatcher.drain_tasks();
            let _ = std::fs::remove_dir_all(&root);
        }),
    );
    println!("ok");
}

async fn scenario(cx: &mut TestAppContext, root: &Path) {
    cx.executor().allow_parking();
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
    });
    let ws = root.join("ws");
    let project = Project::test(fs::RealFs::new(None, cx.executor()), [ws.as_path()], cx).await;
    let page =
        cx.add_window(|window, cx| OmpSettings::new(project.clone(), ws.clone(), window, cx));

    // Every value with the layer that supplies it.
    wait(cx, &page, "first read", |p| {
        p.reloads() >= 1 && !p.settings().is_empty()
    });
    assert_eq!(layer(cx, &page, "display.theme"), Layer::Project);
    assert_eq!(layer(cx, &page, "compaction.enabled"), Layer::Default);
    assert_eq!(
        layer(cx, &page, "tools.approvalMode"),
        Layer::Cedian,
        "the spawn overlay pins it"
    );

    // OMP's CLI changes a setting: the page shows it without a restart.
    let cli = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["config", "set", "compaction.enabled", "false", "--json"])
        .current_dir(&ws)
        .status()
        .unwrap();
    assert!(cli.success());
    wait(cx, &page, "the CLI's change to show", |p| {
        p.setting("compaction.enabled")
            .is_some_and(|s| s.entry.value == Some(json!(false)) && s.layer == Layer::Global)
    });

    // A cedian write is what OMP's CLI reads; a shadowed one says by what.
    page.update(cx, |p, _, cx| p.set_value("display.theme", "solar", cx))
        .unwrap();
    wait(cx, &page, "the shadowed write's message", |p| {
        p.message()
            .is_some_and(|m| m.contains("project layer still wins"))
    });
    assert_eq!(global(root)["display.theme"], json!("solar"));
    assert_eq!(layer(cx, &page, "display.theme"), Layer::Project);

    // OMP refuses a bad value, and the page says so.
    page.update(cx, |p, _, cx| {
        p.set_value("compaction.enabled", "maybe", cx)
    })
    .unwrap();
    wait(cx, &page, "OMP's refusal", |p| {
        p.message()
            .is_some_and(|m| m.contains("OMP refused: Invalid boolean value: maybe"))
    });

    // One model role, written as an entry of the whole record.
    page.update(cx, |p, _, cx| {
        p.set_role("review", "opencode-go/glm-5.3", cx)
    })
    .unwrap();
    wait(cx, &page, "the role to land", |p| {
        p.setting("modelRoles")
            .is_some_and(|s| s.entry.value == Some(json!({"review": "opencode-go/glm-5.3"})))
    });
    assert_eq!(
        global(root)["modelRoles"],
        json!({"review": "opencode-go/glm-5.3"})
    );
}

/// OMP's global config as fake-omp keeps it.
fn global(root: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(root.join("agent/config.yml")).unwrap()).unwrap()
}

fn layer(cx: &mut TestAppContext, page: &WindowHandle<OmpSettings>, key: &str) -> Layer {
    page.update(cx, |p, _, _| p.setting(key).map(|s| s.layer))
        .unwrap()
        .unwrap_or_else(|| panic!("no setting {key}"))
}

/// Pump the test executor while real OS processes and file events land.
fn wait(
    cx: &mut TestAppContext,
    page: &WindowHandle<OmpSettings>,
    what: &str,
    done: impl Fn(&OmpSettings) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        // The file watcher debounces on executor timers: move test time on.
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
        if page.update(cx, |p, _, _| done(p)).unwrap() {
            return;
        }
        let message = page
            .update(cx, |p, _, _| p.message().map(str::to_string))
            .unwrap();
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; message: {message:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

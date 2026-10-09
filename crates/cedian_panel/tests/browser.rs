//! S9 U7, ADR-0049: one Chromium owned by the app. The browser is a fake
//! Chromium process (this binary, launched with `--user-data-dir`), so the
//! lane needs no Chrome.
//!
//! 1. The endpoint OMP is given starts the browser on its first connection,
//!    points discovery back at itself and forwards CDP; dropping the host
//!    closes the endpoint and the browser.
//! 2. Captures carry a frame id and a sequence that run across the
//!    browser's life: capture A, navigate, capture B; evidence on A reads
//!    `stale-frame`, B passes a fresh gate.
//! 3. The person's input wins: input in the window during a turn holds
//!    OMP's next browser message until the person lets the agent continue
//!    or the turn ends; OMP's own `Input.*` and input outside a turn do not.
//! 4. The U7 exit through the real panel, OMP played by fake-omp: the spawn
//!    overlay carries `browser.cdpUrl` (the panel's endpoint) and
//!    `browser.relay: false`; OMP's first connection starts the browser;
//!    capture A, navigate, capture B in the panel: the sequence advances,
//!    the panel shows B inline, evidence on A reads `stale-frame` and B
//!    passes. "Open browser" starts another workspace's browser.
//! 5. `--ignored`: the same capture/navigate/capture against a real
//!    Chromium, when one is installed (it opens a window).
//!
//! Harness off: launched with `--user-data-dir` this binary is the browser;
//! with `--mode` (or `config`) it is fake-omp.

use cedian_panel::browser::{BrowserHost, fake};
use cedian_panel::{CedianPanel, Connection};
use cedian_workflow::{CurrentState, Gate, GateKind, GatePredicate, GateStatus, Outcome};
use gpui::{Modifiers, TestAppContext, VisualTestContext, WindowHandle};
use project::Project;
use serde_json::{Value, json};
use settings::SettingsStore;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if fake::is_launch(&args) {
        std::process::exit(fake::run(&args));
    }
    match args.first().map(String::as_str) {
        Some("--mode") => std::process::exit(cedian_fake_omp::run(&args)),
        Some("config") => std::process::exit(cedian_fake_omp::config_get(&args)),
        _ => {}
    }
    let root = std::env::temp_dir().join(format!("cedian-u7-browser-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("ws")).unwrap();
    std::fs::create_dir_all(root.join("ws2")).unwrap();
    std::fs::write(
        root.join("cedian.toml"),
        "schema = 1\n[[workflow.floor]]\nkind = \"prototype\"\nmin_risk = \"low\"\n\
         gates = [{ id = \"ui\", gate_kind = \"visual\", evidence_kinds = [\"browser\"] }]\n",
    )
    .unwrap();
    let root = root.canonicalize().unwrap();
    // SAFETY: single-threaded here; nothing else reads the environment yet.
    unsafe {
        std::env::set_var("CEDIAN_CONFIG", root.join("cedian.toml"));
        std::env::set_var("CEDIAN_STATE_DIR", root.join("state"));
        std::env::set_var("HOME", root.join("home"));
        std::env::set_var("CEDIAN_OMP_BINARY", std::env::current_exe().unwrap());
        std::env::set_var("CEDIAN_CHROMIUM", std::env::current_exe().unwrap());
    }
    if args.iter().any(|a| a == "--ignored") {
        run("live_chromium_frames", || live_chromium_frames(&root));
        let _ = std::fs::remove_dir_all(&root);
        return;
    }
    run("the_endpoint_refuses_web_pages", || {
        the_endpoint_refuses_web_pages(&root)
    });
    run("first_connection_starts_the_browser", || {
        first_connection_starts_the_browser(&root)
    });
    run("frames_are_bound_to_the_browser", || {
        frames_are_bound_to_the_browser(&root)
    });
    run("every_tab_is_followed", || every_tab_is_followed(&root));
    run("a_capture_binds_the_frame_it_shows", || {
        a_capture_binds_the_frame_it_shows(&root)
    });
    run("a_same_document_navigation_stales_a_capture", || {
        a_same_document_navigation_stales_a_capture(&root)
    });
    run("closing_the_captured_tab_stales_a_capture", || {
        closing_the_captured_tab_stales_a_capture(&root)
    });
    run("a_capture_does_not_relaunch_the_browser", || {
        a_capture_does_not_relaunch_the_browser(&root)
    });
    run("closing_now_closes_the_browser_before_returning", || {
        closing_now_closes_the_browser_before_returning(&root)
    });
    run("drop_during_launch_returns_at_once", || {
        drop_during_launch_returns_at_once(&root)
    });
    run("a_browser_left_by_a_crash_is_closed", || {
        a_browser_left_by_a_crash_is_closed(&root)
    });
    run("a_half_written_port_file_is_waited_for", || {
        a_half_written_port_file_is_waited_for(&root)
    });
    run("the_persons_input_wins", || the_persons_input_wins(&root));
    run("the_person_preempts_continuous_agent_input", || {
        the_person_preempts_continuous_agent_input(&root)
    });
    run("the_person_preempts_dense_agent_typing", || {
        the_person_preempts_dense_agent_typing(&root)
    });
    run("a_page_cannot_forge_the_persons_input", || {
        a_page_cannot_forge_the_persons_input(&root)
    });
    run("an_unanswered_agent_input_ends_with_its_socket", || {
        an_unanswered_agent_input_ends_with_its_socket(&root)
    });
    let exit_root = root.clone();
    run("u7_exit_through_the_panel", move || {
        gpui::run_test_once(
            0,
            Box::new(move |dispatcher| {
                let exec = std::sync::Arc::new(dispatcher.clone());
                let mut cx = TestAppContext::build(dispatcher.clone(), Some("browser"));
                gpui::ForegroundExecutor::new(exec).block_test(exit(&mut cx, &exit_root));
                cx.run_until_parked();
                cx.update(|cx| cx.quit());
                cx.run_until_parked();
                dispatcher.drain_tasks();
            }),
        )
    });
    let _ = std::fs::remove_dir_all(&root);
}

/// Runs `test` unless a name filter (`cargo test --test browser -- NAME`)
/// leaves it out.
fn run(name: &str, test: impl FnOnce()) {
    let filters: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect();
    if !filters.is_empty() && !filters.iter().any(|f| name.contains(f.as_str())) {
        return;
    }
    print!("test {name} ... ");
    test();
    println!("ok");
}

fn exe() -> Option<PathBuf> {
    Some(std::env::current_exe().unwrap())
}

fn get(url: &str, path: &str) -> Result<String, String> {
    let addr = url.trim_start_matches("http://");
    let mut stream = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(70)))
        .unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: {addr}\r\n\r\n").map_err(|e| e.to_string())?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).map_err(|e| e.to_string())?;
    let (head, body) = raw.split_once("\r\n\r\n").ok_or("no body")?;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}\n{body}");
    Ok(body.to_string())
}

fn call(
    socket: &mut tungstenite::WebSocket<impl Read + Write>,
    method: &str,
    params: Value,
) -> Value {
    socket
        .send(tungstenite::Message::Text(
            json!({"id": 1, "method": method, "params": params})
                .to_string()
                .into(),
        ))
        .unwrap();
    loop {
        let message: Value =
            serde_json::from_str(&socket.read().unwrap().into_text().unwrap()).unwrap();
        if message["id"] == 1 {
            return message["result"].clone();
        }
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The raw status line for `request` sent to `url`'s endpoint.
fn raw(url: &str, request: &str) -> String {
    let mut stream = TcpStream::connect(url.trim_start_matches("http://")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut out = Vec::new();
    let _ = stream.read_to_end(&mut out);
    String::from_utf8_lossy(&out)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

fn the_endpoint_refuses_web_pages(root: &std::path::Path) {
    let host = BrowserHost::open(root.join("refuse-profile"), exe(), || {}).unwrap();
    let own = host.url().trim_start_matches("http://").to_string();
    let rebound = raw(
        &host.url(),
        "GET /json/version HTTP/1.1\r\nHost: evil.example\r\n\r\n",
    );
    assert!(
        rebound.starts_with("HTTP/1.1 403"),
        "foreign Host: {rebound}"
    );
    let page = raw(
        &host.url(),
        &format!(
            "GET /devtools/page/P1 HTTP/1.1\r\nHost: {own}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nOrigin: http://evil.example\r\n\r\n"
        ),
    );
    assert!(
        page.starts_with("HTTP/1.1 403"),
        "upgrade with an Origin: {page}"
    );
    let fetch = raw(
        &host.url(),
        &format!("GET /json/list HTTP/1.1\r\nHost: {own}\r\nOrigin: http://evil.example\r\n\r\n"),
    );
    assert!(
        fetch.starts_with("HTTP/1.1 403"),
        "fetch with an Origin: {fetch}"
    );
    let other = raw(
        &host.url(),
        &format!("GET /json/new?https://x.test HTTP/1.1\r\nHost: {own}\r\n\r\n"),
    );
    assert!(
        other.starts_with("HTTP/1.1 404"),
        "only discovery paths: {other}"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(!host.state().running, "a refused request starts nothing");
    host.start().unwrap();
    let local = raw(
        &host.url(),
        &format!(
            "GET /json/version HTTP/1.1\r\nHost: localhost:{}\r\n\r\n",
            own.rsplit(':').next().unwrap()
        ),
    );
    assert!(local.starts_with("HTTP/1.1 200"), "localhost Host: {local}");
}

fn first_connection_starts_the_browser(root: &std::path::Path) {
    let profile = root.join("state/browser-profile");
    let host = BrowserHost::open(profile.clone(), exe(), || {}).unwrap();
    let url = host.url();
    assert!(url.starts_with("http://127.0.0.1:"), "{url}");
    assert!(!host.state().running, "nothing starts before a connection");
    assert!(!profile.join("DevToolsActivePort").exists());

    let version: Value = serde_json::from_str(&get(&url, "/json/version").unwrap()).unwrap();
    assert!(
        host.state().running,
        "the first connection started the browser"
    );
    let own = url.trim_start_matches("http://");
    let browser_ws = version["webSocketDebuggerUrl"].as_str().unwrap();
    assert!(
        browser_ws.starts_with(&format!("ws://{own}/")),
        "discovery points back at cedian: {browser_ws}"
    );
    let chromium_port: u16 = std::fs::read_to_string(profile.join("DevToolsActivePort"))
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .parse()
        .unwrap();

    let list: Value = serde_json::from_str(&get(&url, "/json/list").unwrap()).unwrap();
    let page_ws = list[0]["webSocketDebuggerUrl"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(page_ws.starts_with(&format!("ws://{own}/")), "{page_ws}");
    let (mut socket, _) = tungstenite::connect(&page_ws).unwrap();
    let seq = host.state().seq;
    let result = call(
        &mut socket,
        "Page.navigate",
        json!({"url": "https://a.test/"}),
    );
    assert_eq!(result["frameId"], "F1", "forwarded to the browser");
    wait_until("the navigation reaches cedian's own line", || {
        host.state().seq == seq + 1
    });
    assert_eq!(host.state().url, "https://a.test/");

    drop(socket);
    drop(host);
    wait_until("the browser closes with the host", || {
        TcpStream::connect(("127.0.0.1", chromium_port)).is_err()
    });
    assert!(
        TcpStream::connect(own).is_err() || get(&url, "/json/version").is_err(),
        "the endpoint is closed"
    );
    assert!(profile.exists(), "the profile outlives the browser");
    assert!(
        profile.join("closed-by-cdp").exists(),
        "closed with Browser.close, so cookies flush"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&profile).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "the profile is the person's alone");
    }
}

fn frames_are_bound_to_the_browser(root: &std::path::Path) {
    let host = BrowserHost::open(root.join("frames-profile"), exe(), || {}).unwrap();
    host.start().unwrap();
    let a = host.capture().unwrap();
    assert_eq!(a.png.as_slice(), fake::PNG, "the screenshot");
    assert_eq!(a.frame_id, "F1");
    assert!(a.dom.contains("about:blank"), "the DOM: {}", a.dom);

    let list: Value = serde_json::from_str(&get(&host.url(), "/json/list").unwrap()).unwrap();
    let (mut omp, _) =
        tungstenite::connect(list[0]["webSocketDebuggerUrl"].as_str().unwrap()).unwrap();
    call(&mut omp, "Page.navigate", json!({"url": "https://b.test/"}));
    wait_until("the navigation", || host.state().seq > a.seq);
    let b = host.capture().unwrap();
    assert_eq!(b.seq, a.seq + 1, "the sequence advances across captures");
    assert_eq!(b.url, "https://b.test/");
    assert_eq!(b.console, ["log: loaded https://b.test/"]);
    assert_eq!(b.network, ["200 GET https://b.test/"]);
    assert_eq!(
        host.state().latest.map(|c| c.seq),
        Some(b.seq),
        "the panel shows the latest capture"
    );

    let mut now = CurrentState::from_files([("a.rs".to_string(), b"x".as_slice())]);
    now.frame_seq = Some(host.state().seq);
    let bind = |e: cedian_workflow::Evidence| e.with_code_state(now.bind(&["a.rs".to_string()]));
    let on_a = bind(a.evidence("shot-a", &["looks"], Outcome::Pass));
    let on_b = bind(b.evidence("shot-b", &["looks"], Outcome::Pass));
    let reason = on_a.stale_reason(&now).unwrap();
    assert!(reason.starts_with("stale-frame"), "{reason}");
    let gate = Gate::register(
        "looks",
        GateKind::Visual,
        false,
        GatePredicate {
            kinds: vec![],
            min_items: 1,
            require_ok: true,
            fresh: true,
            feature: None,
        },
        false,
    )
    .unwrap();
    let only_a = gate.evaluate(std::slice::from_ref(&on_a), &now);
    assert_ne!(only_a.status, GateStatus::Passed, "{}", only_a.reason);
    assert!(only_a.reason.contains("1 stale"), "{}", only_a.reason);
    let both = gate.evaluate(&[on_a, on_b], &now);
    assert_eq!(both.status, GateStatus::Passed, "{}", both.reason);
    assert_eq!(both.deciding, ["shot-b"]);
}

fn every_tab_is_followed(root: &std::path::Path) {
    let host = BrowserHost::open(root.join("tabs-profile"), exe(), || {}).unwrap();
    let version: Value = serde_json::from_str(&get(&host.url(), "/json/version").unwrap()).unwrap();
    let (mut browser, _) =
        tungstenite::connect(version["webSocketDebuggerUrl"].as_str().unwrap()).unwrap();
    let created = call(
        &mut browser,
        "Target.createTarget",
        json!({"url": "about:blank"}),
    );
    let second = created["targetId"].as_str().unwrap().to_string();
    let list: Value = serde_json::from_str(&get(&host.url(), "/json/list").unwrap()).unwrap();
    let ws_of = |id: &str| {
        list.as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == id)
            .and_then(|t| t["webSocketDebuggerUrl"].as_str())
            .unwrap()
            .to_string()
    };
    let (mut tab2, _) = tungstenite::connect(ws_of(&second)).unwrap();
    let seq = host.state().seq;
    call(
        &mut tab2,
        "Page.navigate",
        json!({"url": "https://two.test/"}),
    );
    wait_until("a navigation in the new tab advances the sequence", || {
        host.state().seq == seq + 1
    });
    assert_eq!(host.state().url, "https://two.test/");

    call(
        &mut tab2,
        "Fake.childNavigate",
        json!({"url": "https://ad.test/"}),
    );
    let (mut first, _) = tungstenite::connect(ws_of("P1")).unwrap();
    call(
        &mut first,
        "Page.navigate",
        json!({"url": "https://one.test/"}),
    );
    wait_until("the first tab's navigation", || host.state().seq == seq + 2);
    assert_eq!(
        host.state().url,
        "https://one.test/",
        "a child frame does not advance the sequence; the first tab's main frame does"
    );

    call(
        &mut browser,
        "Target.closeTarget",
        json!({"targetId": "P1"}),
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        host.state().running,
        "closing a tab keeps the browser running"
    );
    let capture = host.capture().unwrap();
    assert_eq!(
        capture.url, "https://two.test/",
        "captures re-point to the open tab"
    );
}

fn a_capture_binds_the_frame_it_shows(root: &std::path::Path) {
    let host = BrowserHost::open(root.join("moving-profile"), exe(), || {}).unwrap();
    host.start().unwrap();
    let list: Value = serde_json::from_str(&get(&host.url(), "/json/list").unwrap()).unwrap();
    let (mut omp, _) =
        tungstenite::connect(list[0]["webSocketDebuggerUrl"].as_str().unwrap()).unwrap();
    call(&mut omp, "Fake.navigateMidCapture", json!({"times": 1}));
    let retried = host.capture().unwrap();
    assert_eq!(retried.url, "https://moved.test/0");
    assert_eq!(
        retried.seq,
        host.state().seq,
        "retried at the frame it shows"
    );
    let mut now = CurrentState::default();
    now.frame_seq = Some(host.state().seq);
    let fresh = retried
        .evidence("retried", &[], Outcome::Pass)
        .with_code_state(now.bind(&[]));
    assert_eq!(fresh.stale_reason(&now), None);

    call(&mut omp, "Fake.navigateMidCapture", json!({"times": 100}));
    let moving = host.capture().unwrap();
    now.frame_seq = Some(moving.seq);
    let reason = moving
        .evidence("moving", &[], Outcome::Pass)
        .with_code_state(now.bind(&[]))
        .stale_reason(&now)
        .expect("a page that never holds still gives a capture born stale");
    assert!(reason.starts_with("stale-frame"), "{reason}");
}

fn stale_reason(capture: &cedian_panel::browser::Capture, seq: u64) -> Option<String> {
    let mut now = CurrentState::default();
    now.frame_seq = Some(seq);
    capture
        .evidence("shot", &[], Outcome::Pass)
        .with_code_state(now.bind(&[]))
        .stale_reason(&now)
}

fn a_same_document_navigation_stales_a_capture(root: &std::path::Path) {
    let host = BrowserHost::open(root.join("spa-profile"), exe(), || {}).unwrap();
    host.start().unwrap();
    let list: Value = serde_json::from_str(&get(&host.url(), "/json/list").unwrap()).unwrap();
    let (mut omp, _) =
        tungstenite::connect(list[0]["webSocketDebuggerUrl"].as_str().unwrap()).unwrap();
    let a = host.capture().unwrap();
    call(
        &mut omp,
        "Fake.pushState",
        json!({"url": "about:blank#/route"}),
    );
    wait_until("the route change advances the sequence", || {
        host.state().seq > a.seq
    });
    assert_eq!(host.state().url, "about:blank#/route");
    let reason = stale_reason(&a, host.state().seq).expect("stale after pushState");
    assert!(reason.starts_with("stale-frame"), "{reason}");
}

fn closing_the_captured_tab_stales_a_capture(root: &std::path::Path) {
    let host = BrowserHost::open(root.join("close-tab-profile"), exe(), || {}).unwrap();
    host.start().unwrap();
    let version: Value = serde_json::from_str(&get(&host.url(), "/json/version").unwrap()).unwrap();
    let (mut browser, _) =
        tungstenite::connect(version["webSocketDebuggerUrl"].as_str().unwrap()).unwrap();
    call(
        &mut browser,
        "Target.createTarget",
        json!({"url": "https://other.test/"}),
    );
    let (mut omp, _) = tungstenite::connect(list_ws(&host, "P1")).unwrap();
    call(
        &mut omp,
        "Page.navigate",
        json!({"url": "https://shown.test/"}),
    );
    wait_until("the captured tab is active", || {
        host.state().url == "https://shown.test/"
    });
    let a = host.capture().unwrap();
    assert_eq!(a.url, "https://shown.test/");
    call(
        &mut browser,
        "Target.closeTarget",
        json!({"targetId": "P1"}),
    );
    wait_until("closing the tab advances the sequence", || {
        host.state().seq > a.seq
    });
    let reason = stale_reason(&a, host.state().seq).expect("stale after the tab closed");
    assert!(reason.starts_with("stale-frame"), "{reason}");
}

fn closing_now_closes_the_browser_before_returning(root: &std::path::Path) {
    let profile = root.join("quit-profile");
    let host = BrowserHost::open(profile.clone(), exe(), || {}).unwrap();
    host.start().unwrap();
    let pid = std::fs::read_to_string(profile.join("fake.pid")).unwrap();
    host.close_now(Duration::from_secs(2));
    assert!(
        profile.join("closed-by-cdp").exists(),
        "closed over CDP, so cookies flush"
    );
    assert!(!alive(&pid), "the browser is gone when close_now returns");
    assert!(!host.state().running);
    assert!(host.start().is_err(), "a closed host does not relaunch");
}

/// A capture after OMP's call must show the page that call left: when the
/// browser has gone, it fails instead of starting a fresh one.
#[allow(clippy::disallowed_methods, reason = "a test probe")]
fn a_capture_does_not_relaunch_the_browser(root: &std::path::Path) {
    let profile = root.join("gone-profile");
    let host = BrowserHost::open(profile.clone(), exe(), || {}).unwrap();
    host.start().unwrap();
    let pid = std::fs::read_to_string(profile.join("fake.pid")).unwrap();
    std::process::Command::new("kill")
        .args(["-9", pid.trim()])
        .status()
        .unwrap();
    wait_until("the browser is gone", || !host.state().running);
    let refused = host.capture().map(|c| c.seq).unwrap_err();
    assert!(refused.contains("not running"), "{refused}");
    assert!(!host.state().running, "the capture started no browser");
}

fn list_ws(host: &BrowserHost, id: &str) -> String {
    let list: Value = serde_json::from_str(&get(&host.url(), "/json/list").unwrap()).unwrap();
    list.as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == id)
        .and_then(|t| t["webSocketDebuggerUrl"].as_str())
        .unwrap()
        .to_string()
}

#[allow(clippy::disallowed_methods, reason = "a test probe")]
fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn drop_during_launch_returns_at_once(root: &std::path::Path) {
    let profile = root.join("slow-profile");
    // SAFETY: the tests run one at a time; the fake reads it at launch.
    unsafe { std::env::set_var("CEDIAN_FAKE_BROWSER_DELAY_MS", "3000") };
    let host = BrowserHost::open(profile.clone(), exe(), || {}).unwrap();
    let url = host.url();
    let opener = std::thread::spawn(move || {
        let _ = raw(
            &url,
            &format!(
                "GET /json/version HTTP/1.1\r\nHost: {}\r\n\r\n",
                url.trim_start_matches("http://")
            ),
        );
    });
    wait_until("the launch begins", || profile.join("fake.pid").exists());
    unsafe { std::env::remove_var("CEDIAN_FAKE_BROWSER_DELAY_MS") };
    let pid = std::fs::read_to_string(profile.join("fake.pid")).unwrap();
    let started = Instant::now();
    drop(host);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "drop waited on the launch: {:?}",
        started.elapsed()
    );
    let _ = opener.join();
    wait_until("the late Chromium is killed", || !alive(&pid));
}

#[allow(
    clippy::disallowed_methods,
    reason = "a test stands in for a crashed cedian's browser"
)]
fn a_browser_left_by_a_crash_is_closed(root: &std::path::Path) {
    let profile = root.join("crash-profile");
    std::fs::create_dir_all(&profile).unwrap();
    let mut orphan = std::process::Command::new(std::env::current_exe().unwrap())
        .arg(format!("--user-data-dir={}", profile.display()))
        .spawn()
        .unwrap();
    wait_until("the orphan opens its port", || {
        profile.join("DevToolsActivePort").exists()
    });
    let host = BrowserHost::open(profile.clone(), exe(), || {}).unwrap();
    host.start().unwrap();
    wait_until("the orphan is closed", || {
        orphan.try_wait().unwrap().is_some()
    });
    assert!(
        profile.join("closed-by-cdp").exists(),
        "closed over CDP, not killed"
    );
    assert!(host.state().running, "a fresh browser runs on the profile");
}

/// Chromium creates `DevToolsActivePort` before writing it; a launch that
/// reads it in between still finds the browser a crash left, even when the
/// write lands half a second later (a CI runner's scheduling slack).
#[allow(
    clippy::disallowed_methods,
    reason = "a test stands in for a crashed cedian's browser"
)]
fn a_half_written_port_file_is_waited_for(root: &std::path::Path) {
    let profile = root.join("half-written-profile");
    std::fs::create_dir_all(&profile).unwrap();
    let mut orphan = std::process::Command::new(std::env::current_exe().unwrap())
        .arg(format!("--user-data-dir={}", profile.display()))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let port_file = profile.join("DevToolsActivePort");
    wait_until("the orphan opens its port", || {
        std::fs::read_to_string(&port_file).is_ok_and(|t| !t.is_empty())
    });
    let written = std::fs::read_to_string(&port_file).unwrap();
    std::fs::write(&port_file, "").unwrap();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        std::fs::write(&port_file, written).unwrap();
    });
    let host = BrowserHost::open(profile, exe(), || {}).unwrap();
    let _ = host.start();
    writer.join().unwrap();
    wait_until("the orphan is closed", || {
        orphan.try_wait().unwrap().is_some()
    });
}

fn send(socket: &mut tungstenite::WebSocket<impl Read + Write>, id: u64, method: &str) {
    send_params(socket, id, method, json!({}));
}

fn send_params(
    socket: &mut tungstenite::WebSocket<impl Read + Write>,
    id: u64,
    method: &str,
    params: Value,
) {
    socket
        .send(tungstenite::Message::Text(
            json!({"id": id, "method": method, "params": params})
                .to_string()
                .into(),
        ))
        .unwrap();
}

/// The reply to `id` within `limit`, skipping events.
fn reply(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    id: u64,
    limit: Duration,
) -> Option<Value> {
    if let tungstenite::stream::MaybeTlsStream::Plain(tcp) = socket.get_ref() {
        tcp.set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
    }
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Ok(message) = socket.read() {
            let message: Value = serde_json::from_str(&message.into_text().unwrap()).unwrap();
            if message["id"] == id {
                return Some(message);
            }
        }
    }
    None
}

fn the_persons_input_wins(root: &std::path::Path) {
    let profile = root.join("input-profile");
    let host = BrowserHost::open(profile.clone(), exe(), || {}).unwrap();
    host.start().unwrap();
    let list: Value = serde_json::from_str(&get(&host.url(), "/json/list").unwrap()).unwrap();
    let (mut omp, _) =
        tungstenite::connect(list[0]["webSocketDebuggerUrl"].as_str().unwrap()).unwrap();
    let port = std::fs::read_to_string(profile.join("DevToolsActivePort")).unwrap();
    let port = port.lines().next().unwrap();
    let (mut person, _) =
        tungstenite::connect(format!("ws://127.0.0.1:{port}/devtools/page/P1")).unwrap();
    let pause = Duration::from_millis(300);

    send(&mut person, 1, "Fake.personInput");
    std::thread::sleep(pause);
    assert!(
        !host.state().preempted,
        "outside a turn the person's input holds nothing"
    );

    host.set_turn(true);
    send_params(
        &mut omp,
        2,
        "Input.dispatchMouseEvent",
        json!({"type": "mousePressed"}),
    );
    assert!(reply(&mut omp, 2, Duration::from_secs(5)).is_some());
    std::thread::sleep(pause);
    assert!(
        !host.state().preempted,
        "OMP's own input is not the person's"
    );

    std::thread::sleep(Duration::from_millis(1100));
    send(&mut person, 3, "Fake.personInput");
    wait_until("the person's input is seen", || host.state().preempted);
    send(&mut omp, 4, "Page.navigate");
    assert!(
        reply(&mut omp, 4, Duration::from_millis(500)).is_none(),
        "OMP's next browser call is held"
    );
    host.resume();
    assert!(
        reply(&mut omp, 4, Duration::from_secs(5)).is_some(),
        "released when the person lets the agent continue"
    );

    std::thread::sleep(Duration::from_millis(1100));
    send(&mut person, 5, "Fake.personInput");
    wait_until("held again", || host.state().preempted);
    send(&mut omp, 6, "Page.navigate");
    assert!(reply(&mut omp, 6, Duration::from_millis(500)).is_none());
    host.set_turn(false);
    assert!(
        reply(&mut omp, 6, Duration::from_secs(5)).is_some(),
        "released when the turn ends"
    );
    assert!(!host.state().preempted);
}

fn the_person_preempts_continuous_agent_input(root: &std::path::Path) {
    let profile = root.join("typing-profile");
    let host = BrowserHost::open(profile.clone(), exe(), || {}).unwrap();
    host.start().unwrap();
    let list: Value = serde_json::from_str(&get(&host.url(), "/json/list").unwrap()).unwrap();
    let page_ws = list[0]["webSocketDebuggerUrl"]
        .as_str()
        .unwrap()
        .to_string();
    let port = std::fs::read_to_string(profile.join("DevToolsActivePort")).unwrap();
    let port = port.lines().next().unwrap().to_string();
    let (mut person, _) =
        tungstenite::connect(format!("ws://127.0.0.1:{port}/devtools/page/P1")).unwrap();
    host.set_turn(true);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let agent = std::thread::spawn({
        let stop = stop.clone();
        move || {
            let (mut omp, _) = tungstenite::connect(&page_ws).unwrap();
            let mut id = 100;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                send_params(
                    &mut omp,
                    id,
                    "Input.dispatchKeyEvent",
                    json!({"type": "keyDown"}),
                );
                reply(&mut omp, id, Duration::from_millis(200));
                id += 1;
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    });
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !host.state().preempted,
        "the agent's own typing is not the person's"
    );
    for id in 0..3 {
        send(&mut person, id, "Fake.personInput");
        std::thread::sleep(Duration::from_millis(70));
    }
    wait_until("the person preempts the typing agent", || {
        host.state().preempted
    });
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    host.set_turn(false);
    agent.join().unwrap();
}

/// The page socket OMP would use and a direct line standing in for the
/// person's hand on the window, on a running browser in a turn.
fn turn_with_person(
    host: &BrowserHost,
    profile: &Path,
) -> (
    String,
    tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
) {
    host.start().unwrap();
    let list: Value = serde_json::from_str(&get(&host.url(), "/json/list").unwrap()).unwrap();
    let page_ws = list[0]["webSocketDebuggerUrl"]
        .as_str()
        .unwrap()
        .to_string();
    let port = std::fs::read_to_string(profile.join("DevToolsActivePort")).unwrap();
    let port = port.lines().next().unwrap().to_string();
    let (person, _) =
        tungstenite::connect(format!("ws://127.0.0.1:{port}/devtools/page/P1")).unwrap();
    host.set_turn(true);
    (page_ws, person)
}

fn the_person_preempts_dense_agent_typing(root: &std::path::Path) {
    let profile = root.join("dense-profile");
    let host = BrowserHost::open(profile.clone(), exe(), || {}).unwrap();
    let (page_ws, mut person) = turn_with_person(&host, &profile);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let agent = std::thread::spawn({
        let stop = stop.clone();
        move || {
            let (mut omp, _) = tungstenite::connect(&page_ws).unwrap();
            let mut id = 100;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                for kind in ["keyDown", "keyUp"] {
                    send_params(
                        &mut omp,
                        id,
                        "Input.dispatchKeyEvent",
                        json!({"type": kind}),
                    );
                    if reply(&mut omp, id, Duration::from_millis(200)).is_none() {
                        return;
                    }
                    id += 1;
                }
            }
        }
    });
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !host.state().preempted,
        "the agent's own back-to-back keys are not the person's"
    );
    send(&mut person, 1, "Fake.personInput");
    let deadline = Instant::now() + Duration::from_secs(1);
    while !host.state().preempted {
        assert!(
            Instant::now() < deadline,
            "the person's click during dense typing was taken as the agent's"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    host.resume();
    send_params(
        &mut person,
        2,
        "Fake.personInput",
        json!({"type": "keydown"}),
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    while !host.state().preempted {
        assert!(
            Instant::now() < deadline,
            "the person's key during dense typing was taken as the agent's: one \
             dispatchKeyEvent claims one keydown"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    host.set_turn(false);
    agent.join().unwrap();
}

fn a_page_cannot_forge_the_persons_input(root: &std::path::Path) {
    let profile = root.join("forge-profile");
    let host = BrowserHost::open(profile.clone(), exe(), || {}).unwrap();
    let (_, mut page) = turn_with_person(&host, &profile);
    for name in ["cedianInput", "__cedianInput"] {
        send_params(
            &mut page,
            1,
            "Fake.pageScriptCalls",
            json!({"name": name, "payload": {"type": "pointerdown"}}),
        );
        assert!(reply(&mut page, 1, Duration::from_secs(5)).is_some());
    }
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !host.state().preempted,
        "a page script calling a guessable binding preempted the agent"
    );
    send(&mut page, 2, "Fake.personInput");
    wait_until("the person's real input still preempts", || {
        host.state().preempted
    });
}

fn an_unanswered_agent_input_ends_with_its_socket(root: &std::path::Path) {
    let profile = root.join("unanswered-profile");
    let host = BrowserHost::open(profile.clone(), exe(), || {}).unwrap();
    let (page_ws, mut person) = turn_with_person(&host, &profile);
    let (mut omp, _) = tungstenite::connect(&page_ws).unwrap();
    send_params(
        &mut omp,
        7,
        "Input.dispatchMouseEvent",
        json!({"type": "mousePressed", "fakeNoReply": true}),
    );
    std::thread::sleep(Duration::from_millis(200));
    drop(omp);
    std::thread::sleep(Duration::from_millis(300));
    send(&mut person, 1, "Fake.personInput");
    wait_until(
        "the person preempts once the dead socket's input is gone",
        || host.state().preempted,
    );
}

const LAUNCH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/app_launch.jsonl"
);

async fn exit(cx: &mut TestAppContext, root: &Path) {
    cx.executor().allow_parking();
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
    });
    let ws = root.join("ws");
    let omp_dir = cedian_shell::state::dir(&ws).unwrap().join("omp");
    cedian_fake_omp::install_replay(&omp_dir, Path::new(LAUNCH)).unwrap();
    let project = Project::test(fs::RealFs::new(None, cx.executor()), [ws.as_path()], cx).await;
    let window = cx.add_window(|window, cx| CedianPanel::new(project.clone(), window, cx));
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    wait_ready(cx, &window);

    let host = window
        .update(cx, |p, _, _| p.browser().cloned())
        .unwrap()
        .expect("the panel opened the workspace's browser endpoint");
    let overlay: Value =
        serde_json::from_str(&std::fs::read_to_string(omp_dir.join("cedian-overlay.yml")).unwrap())
            .unwrap();
    assert_eq!(
        overlay["browser"],
        json!({"cdpUrl": host.url(), "relay": false}),
        "OMP is pointed at the app's browser"
    );
    assert!(!host.state().running, "nothing starts before OMP connects");

    let version: Value = serde_json::from_str(&get(&host.url(), "/json/version").unwrap()).unwrap();
    assert!(version["webSocketDebuggerUrl"].is_string());
    assert!(
        host.state().running,
        "OMP's first connection started the browser"
    );

    window.update(cx, |p, _, cx| p.capture_browser(cx)).unwrap();
    let a = wait_capture(cx, &window, 0);
    let list: Value = serde_json::from_str(&get(&host.url(), "/json/list").unwrap()).unwrap();
    let (mut omp, _) =
        tungstenite::connect(list[0]["webSocketDebuggerUrl"].as_str().unwrap()).unwrap();
    call(
        &mut omp,
        "Page.navigate",
        json!({"url": "https://exit.test/"}),
    );
    wait_until("the navigation", || host.state().seq > a.seq);
    let gate = Gate::register(
        cedian_panel::BROWSER_GATE,
        GateKind::Visual,
        false,
        GatePredicate {
            kinds: vec![],
            min_items: 1,
            require_ok: true,
            fresh: true,
            feature: None,
        },
        false,
    )
    .unwrap();
    let only_a = window.update(cx, |p, _, _| p.evaluate_gate(&gate)).unwrap();
    assert_ne!(only_a.status, GateStatus::Passed, "{}", only_a.reason);
    assert!(
        only_a.reason.contains("1 stale"),
        "A is stale-frame: {}",
        only_a.reason
    );
    window.update(cx, |p, _, cx| p.capture_browser(cx)).unwrap();
    let b = wait_capture(cx, &window, a.seq);
    assert_eq!(
        b.seq,
        a.seq + 1,
        "the frame sequence advances across captures"
    );
    assert!(
        rendered(&mut vcx, "cedian-browser-capture").is_some(),
        "the latest capture is shown inline"
    );
    let wait_gate = |cx: &mut TestAppContext| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let result = window.update(cx, |p, _, _| p.evaluate_gate(&gate)).unwrap();
            if result.status == GateStatus::Passed || Instant::now() > deadline {
                return result;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let both = wait_gate(cx);
    assert_eq!(both.status, GateStatus::Passed, "{}", both.reason);
    assert_eq!(
        both.deciding.len(),
        1,
        "only B decides: {:?}",
        both.deciding
    );
    assert!(
        both.deciding[0].ends_with(&b.seq.to_string()),
        "{:?}",
        both.deciding
    );

    omps_browser_call_is_gate_evidence(cx, &window, &host, &ws, &mut omp);

    let ws2 = root.join("ws2");
    cedian_fake_omp::install_replay(
        &cedian_shell::state::dir(&ws2).unwrap().join("omp"),
        Path::new(LAUNCH),
    )
    .unwrap();
    let project2 = Project::test(fs::RealFs::new(None, cx.executor()), [ws2.as_path()], cx).await;
    let window2 = cx.add_window(|window, cx| CedianPanel::new(project2.clone(), window, cx));
    let mut vcx2 = VisualTestContext::from_window(window2.into(), cx);
    wait_ready(cx, &window2);
    let host2 = window2
        .update(cx, |p, _, _| p.browser().cloned())
        .unwrap()
        .unwrap();
    assert_ne!(host2.url(), host.url(), "one browser per workspace");
    assert!(!host2.state().running);
    let bounds = rendered(&mut vcx2, "cedian-open-browser").expect("Open browser button");
    vcx2.simulate_click(bounds.center(), Modifiers::none());
    let deadline = Instant::now() + Duration::from_secs(30);
    while !host2.state().running {
        assert!(Instant::now() < deadline, "Open browser started nothing");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(20));
    }

    let profile_of = |ws: &Path| {
        cedian_shell::state::dir(ws)
            .unwrap()
            .join("browser-profile")
    };
    cx.quit();
    for (ws, host) in [(&ws, &host), (&ws2, &host2)] {
        assert!(
            profile_of(ws).join("closed-by-cdp").exists(),
            "quitting the app closes {} over CDP",
            ws.display()
        );
        assert!(!host.state().running, "before the app exits");
    }
}

/// U9h (ADR-0055): the end of OMP's `browser` call is a capture, stored as
/// evidence attributed to that call, for the gate the user's floor asks
/// for (`ui`, kind `browser`). Outside a workflow nothing is captured. A
/// navigation after it makes the evidence `stale-frame` and
/// `cedian_complete` refuses.
fn omps_browser_call_is_gate_evidence(
    cx: &mut TestAppContext,
    window: &WindowHandle<CedianPanel>,
    host: &BrowserHost,
    ws: &Path,
    omp: &mut tungstenite::WebSocket<impl Read + Write>,
) {
    let state_dir = cedian_shell::state::dir(ws).unwrap();
    let browser_end = |id: &str| cedian_omp::RouterEvent::ToolEnd {
        tool_call_id: id.to_string(),
        tool_name: "browser".to_string(),
        result_summary: String::new(),
        is_error: false,
        before: Vec::new(),
    };
    let seen = host.state().latest.map(|c| c.seq);
    window
        .update(cx, |p, _, cx| p.router_event(browser_end("fast-lane"), cx))
        .unwrap();
    cx.run_until_parked();
    std::thread::sleep(Duration::from_millis(200));
    cx.run_until_parked();
    assert!(
        !cedian_shell::workflow_store::exists(&state_dir),
        "no workflow, nothing stored"
    );
    assert_eq!(
        host.state().latest.map(|c| c.seq),
        seen,
        "the fast lane takes no capture"
    );

    let channel = window
        .update(cx, |p, _, _| p.workflow_channel())
        .unwrap()
        .expect("the panel keeps the workflow channel");
    let args = |v: Value| v.as_object().unwrap().clone();
    channel
        .update(&args(
            json!({"op": "start", "kind": "prototype", "title": "ui", "risk": "low"}),
        ))
        .unwrap();
    window
        .update(cx, |p, _, cx| {
            p.router_event(browser_end("omp-browser-1"), cx)
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let item = loop {
        cx.run_until_parked();
        let state = cedian_shell::workflow_store::load(&state_dir).unwrap();
        if let Some(item) = state.evidence.values().find(|e| e.for_gates == ["ui"]) {
            break item.clone();
        }
        assert!(
            Instant::now() < deadline,
            "no evidence for the ui gate from OMP's browser call: {:?}",
            state.evidence
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(
        item.provenance,
        cedian_workflow::Provenance::Attributed {
            task_id: cedian_panel::TASK_ID.to_string(),
            tool_call_id: "omp-browser-1".to_string(),
        }
    );
    assert_eq!(item.kind, cedian_workflow::EvidenceKind::Browser);
    let at = host.state().seq;
    assert_eq!(item.frame_seq, Some(at), "bound to the frame captured");
    let state = cedian_shell::workflow_store::load(&state_dir).unwrap();
    let ui = state.gate_result("ui", &channel.current()).unwrap();
    assert_eq!(ui.status, GateStatus::Passed, "{}", ui.reason);

    call(omp, "Page.navigate", json!({"url": "https://after.test/"}));
    wait_until("the navigation", || host.state().seq > at);
    let stale = item.stale_reason(&channel.current()).unwrap_or_default();
    assert!(stale.starts_with("stale-frame"), "{stale:?}");
    let refused = channel.complete(&args(json!({}))).unwrap_err();
    assert!(refused.contains("ui"), "{refused}");
}

fn rendered(
    vcx: &mut VisualTestContext,
    selector: &'static str,
) -> Option<gpui::Bounds<gpui::Pixels>> {
    vcx.update(|window, _| window.refresh());
    vcx.run_until_parked();
    vcx.debug_bounds(selector)
}

fn wait_ready(cx: &mut TestAppContext, window: &WindowHandle<CedianPanel>) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        cx.run_until_parked();
        let connection = window.update(cx, |p, _, _| p.connection().clone()).unwrap();
        match connection {
            Connection::Ready { .. } => return,
            Connection::Stopped(e) => panic!("OMP stopped: {e}"),
            _ => {}
        }
        assert!(Instant::now() < deadline, "timed out waiting for OMP");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_capture(
    cx: &mut TestAppContext,
    window: &WindowHandle<CedianPanel>,
    after: u64,
) -> cedian_panel::browser::Capture {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        cx.run_until_parked();
        let latest = window
            .update(cx, |p, _, _| p.browser().and_then(|b| b.state().latest))
            .unwrap();
        if let Some(capture) = latest.filter(|c| c.seq > after) {
            return capture;
        }
        assert!(Instant::now() < deadline, "timed out waiting for a capture");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn live_chromium_frames(root: &Path) {
    // SAFETY: single-threaded here.
    unsafe { std::env::remove_var("CEDIAN_CHROMIUM") };
    let Some(chromium) = cedian_panel::browser::executable() else {
        println!("skipped: no Chrome or Chromium installed");
        return;
    };
    let host = BrowserHost::open(root.join("live-profile"), Some(chromium), || {}).unwrap();
    let list: Value = serde_json::from_str(&get(&host.url(), "/json/list").unwrap()).unwrap();
    let page = list
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["type"] == "page")
        .unwrap();
    let (mut omp, _) =
        tungstenite::connect(page["webSocketDebuggerUrl"].as_str().unwrap()).unwrap();
    call(
        &mut omp,
        "Page.navigate",
        json!({"url": "data:text/html,<title>a</title><script>console.log('hello a')</script>"}),
    );
    wait_until("page a", || host.state().url.starts_with("data:"));
    let a = host.capture().unwrap();
    assert!(a.png.starts_with(b"\x89PNG"), "a real screenshot");
    assert_eq!(a.title, "a");
    call(
        &mut omp,
        "Page.navigate",
        json!({"url": "data:text/html,<title>b</title>"}),
    );
    wait_until("page b", || host.state().seq > a.seq);
    let b = host.capture().unwrap();
    assert_eq!(b.title, "b");
    assert!(b.seq > a.seq, "the frame sequence advances across captures");
    let mut now = CurrentState::default();
    now.frame_seq = Some(host.state().seq);
    assert!(
        a.evidence("a", &[], Outcome::Pass)
            .stale_reason(&now)
            .unwrap()
            .starts_with("stale-frame")
    );
    println!(
        "(console on a: {:?}; seq {} -> {})",
        a.console, a.seq, b.seq
    );
}

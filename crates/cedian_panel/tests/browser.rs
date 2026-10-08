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
    std::fs::write(root.join("cedian.toml"), "schema = 1\n").unwrap();
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
    run("the_persons_input_wins", || the_persons_input_wins(&root));
    run("the_person_preempts_continuous_agent_input", || {
        the_person_preempts_continuous_agent_input(&root)
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

fn run(name: &str, test: impl FnOnce()) {
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
}

fn frames_are_bound_to_the_browser(root: &std::path::Path) {
    let host = BrowserHost::open(root.join("frames-profile"), exe(), || {}).unwrap();
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

fn send(socket: &mut tungstenite::WebSocket<impl Read + Write>, id: u64, method: &str) {
    socket
        .send(tungstenite::Message::Text(
            json!({"id": id, "method": method, "params": {}})
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
    send(&mut omp, 2, "Input.dispatchMouseEvent");
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
                send(&mut omp, id, "Input.dispatchKeyEvent");
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

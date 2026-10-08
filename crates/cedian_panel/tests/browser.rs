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
//!
//! Harness off: launched with `--user-data-dir` this binary is the browser.

use cedian_panel::browser::{BrowserHost, fake};
use cedian_workflow::{CurrentState, Gate, GateKind, GatePredicate, GateStatus, Outcome};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if fake::is_launch(&args) {
        std::process::exit(fake::run(&args));
    }
    let root = std::env::temp_dir().join(format!("cedian-u7-browser-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    run("first_connection_starts_the_browser", || {
        first_connection_starts_the_browser(&root)
    });
    run("frames_are_bound_to_the_browser", || {
        frames_are_bound_to_the_browser(&root)
    });
    run("the_persons_input_wins", || the_persons_input_wins(&root));
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

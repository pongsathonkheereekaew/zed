//! S9 U7, ADR-0049: one Chromium owned by the app. The browser is a fake
//! Chromium process (this binary, launched with `--user-data-dir`), so the
//! lane needs no Chrome.
//!
//! 1. The endpoint OMP is given starts the browser on its first connection,
//!    points discovery back at itself and forwards CDP; dropping the host
//!    closes the endpoint and the browser.
//!
//! Harness off: launched with `--user-data-dir` this binary is the browser.

use cedian_panel::browser::{BrowserHost, fake};
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

//! A stand-in Chromium for hermetic tests: a process that writes
//! `DevToolsActivePort` into its `--user-data-dir` and answers CDP discovery
//! and the CDP methods cedian and a Puppeteer-style client use, with one
//! page whose navigations emit `Page.frameNavigated`, a console line and a
//! network exchange. `Fake.personInput` stands in for the person clicking
//! in the window; any `Input.*` command fires the page's input listener too,
//! as real CDP input does.

use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, mpsc};
use std::time::Duration;
use tungstenite::Message;

/// A 1x1 PNG.
pub const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

#[derive(Default)]
struct Page {
    url: String,
    listeners: Vec<mpsc::Sender<String>>,
    request: u64,
}

/// Whether `args` are a browser launch rather than a test run.
pub fn is_launch(args: &[String]) -> bool {
    args.iter().any(|a| a.starts_with("--user-data-dir="))
}

/// Serve until killed.
pub fn run(args: &[String]) -> i32 {
    let profile = args
        .iter()
        .find_map(|a| a.strip_prefix("--user-data-dir="))
        .expect("--user-data-dir");
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::fs::write(
        std::path::Path::new(profile).join("DevToolsActivePort"),
        format!("{port}\n/devtools/browser/fake\n"),
    )
    .expect("DevToolsActivePort");
    let page = Arc::new(Mutex::new(Page {
        url: "about:blank".to_string(),
        ..Page::default()
    }));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let page = page.clone();
        std::thread::spawn(move || {
            let _ = connection(stream, port, &page);
        });
    }
    0
}

fn connection(stream: TcpStream, port: u16, page: &Arc<Mutex<Page>>) -> Result<(), String> {
    let mut buf = [0u8; 4096];
    let n = stream.peek(&mut buf).map_err(|e| e.to_string())?;
    let head = String::from_utf8_lossy(&buf[..n]).to_lowercase();
    if head.contains("upgrade: websocket") {
        return websocket(stream, page);
    }
    let mut stream = stream;
    let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
    let head = String::from_utf8_lossy(&buf[..n]);
    let path = head.split_whitespace().nth(1).unwrap_or("/");
    let ws = format!("ws://127.0.0.1:{port}");
    let body = match path {
        "/json/version" => json!({
            "Browser": "FakeChromium/1",
            "webSocketDebuggerUrl": format!("{ws}/devtools/browser/fake"),
        }),
        "/json/list" | "/json" => json!([{
            "id": "P1",
            "type": "page",
            "url": page.lock().url,
            "webSocketDebuggerUrl": format!("{ws}/devtools/page/P1"),
        }]),
        _ => json!({}),
    }
    .to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .map_err(|e| e.to_string())
}

fn websocket(stream: TcpStream, page: &Arc<Mutex<Page>>) -> Result<(), String> {
    let mut socket = tungstenite::accept(stream).map_err(|e| e.to_string())?;
    socket
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(5)))
        .map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::channel();
    page.lock().listeners.push(tx);
    loop {
        match socket.read() {
            Ok(Message::Text(text)) => {
                let request: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
                let result = answer(&request, page);
                let reply = json!({"id": request["id"], "result": result});
                socket
                    .send(Message::Text(reply.to_string().into()))
                    .map_err(|e| e.to_string())?;
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return Err(e.to_string()),
        }
        while let Ok(event) = rx.try_recv() {
            socket
                .send(Message::Text(event.into()))
                .map_err(|e| e.to_string())?;
        }
    }
}

fn broadcast(page: &mut Page, method: &str, params: Value) {
    let event = json!({"method": method, "params": params}).to_string();
    page.listeners.retain(|l| l.send(event.clone()).is_ok());
}

fn answer(request: &Value, page: &Arc<Mutex<Page>>) -> Value {
    let params = &request["params"];
    let mut page = page.lock();
    match request["method"].as_str().unwrap_or_default() {
        "Page.navigate" => {
            let url = params["url"].as_str().unwrap_or_default().to_string();
            page.url = url.clone();
            page.request += 1;
            let id = page.request.to_string();
            broadcast(
                &mut page,
                "Network.requestWillBeSent",
                json!({"requestId": id, "request": {"method": "GET", "url": url}}),
            );
            broadcast(
                &mut page,
                "Page.frameNavigated",
                json!({"frame": {"id": "F1", "url": url}}),
            );
            broadcast(
                &mut page,
                "Network.responseReceived",
                json!({"requestId": id, "response": {"url": url, "status": 200}}),
            );
            broadcast(
                &mut page,
                "Runtime.consoleAPICalled",
                json!({"type": "log", "args": [{"type": "string", "value": format!("loaded {url}")}]}),
            );
            json!({"frameId": "F1"})
        }
        "Page.getFrameTree" => json!({"frameTree": {"frame": {"id": "F1", "url": page.url}}}),
        "Page.captureScreenshot" => {
            json!({"data": base64::engine::general_purpose::STANDARD.encode(PNG)})
        }
        "Runtime.evaluate" => {
            let html = format!("<html><body>{}</body></html>", page.url);
            json!({"result": {"type": "object", "value": {"url": page.url, "title": "fake", "html": html}}})
        }
        "Fake.personInput" => {
            broadcast(
                &mut page,
                "Runtime.bindingCalled",
                json!({"name": "cedianInput", "payload": "pointerdown"}),
            );
            json!({})
        }
        method if method.starts_with("Input.") => {
            broadcast(
                &mut page,
                "Runtime.bindingCalled",
                json!({"name": "cedianInput", "payload": "pointerdown"}),
            );
            json!({})
        }
        _ => json!({}),
    }
}

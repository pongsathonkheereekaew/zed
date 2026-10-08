//! A stand-in Chromium for hermetic tests: a process that writes
//! `DevToolsActivePort` into its `--user-data-dir` and answers CDP discovery
//! and the CDP methods cedian and a Puppeteer-style client use. It starts
//! with one page, `P1`; `Target.createTarget` and `Target.closeTarget` open
//! and close more, and a browser connection that asked for
//! `Target.setAutoAttach` gets a flat session on each. A page's navigations
//! emit `Page.frameNavigated`, a console line and a network exchange.
//! `Fake.personInput {type?}` stands in for the person using the window
//! (a `pointerdown` unless `type` says otherwise), `Fake.childNavigate` for
//! a child frame navigating, `Fake.pushState {url}` for a
//! same-document navigation (`history.pushState`), and `Fake.pageScriptCalls {name, payload}` for
//! a page's own script calling a binding by name, which reaches only a
//! binding added without an `executionContextName`. An `Input.*` command
//! fires the page's input listener with the events real CDP input makes
//! (`keyDown` a keydown, `mousePressed` a pointerdown, `mouseWheel` a
//! wheel, `touchStart` a pointerdown per touch point); one with
//! `fakeNoReply` is never answered, as when Chromium hangs.
//! `Fake.navigateMidCapture {times}` navigates the page during each of the
//! next `times` screenshots. `Browser.close` exits, leaving `closed-by-cdp` in the profile;
//! `CEDIAN_FAKE_BROWSER_DELAY_MS` delays opening the debugging port.

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

struct Page {
    url: String,
    frame: String,
    /// Captures still to be interrupted by a navigation (`Fake.navigateMidCapture`).
    navigate_mid_capture: u64,
}

/// Who an event goes to: a page socket, or a flat session on a browser
/// socket.
struct Listener {
    target: String,
    session: Option<String>,
    tx: mpsc::Sender<String>,
}

#[derive(Default)]
struct Browser {
    pages: std::collections::BTreeMap<String, Page>,
    next_page: u64,
    next_session: u64,
    request: u64,
    listeners: Vec<Listener>,
    /// Browser sockets that asked for auto-attach or target discovery.
    auto_attach: Vec<mpsc::Sender<String>>,
    discover: Vec<mpsc::Sender<String>>,
    /// `Runtime.addBinding` names and the world each is visible in.
    bindings: Vec<(String, Option<String>)>,
    profile: std::path::PathBuf,
}

impl Browser {
    fn open_page(&mut self, url: &str) -> String {
        self.next_page += 1;
        let id = format!("P{}", self.next_page);
        self.pages.insert(
            id.clone(),
            Page {
                url: url.to_string(),
                frame: format!("F{}", self.next_page),
                navigate_mid_capture: 0,
            },
        );
        let info = json!({"targetId": id, "type": "page", "url": url});
        let created = event(None, "Target.targetCreated", json!({"targetInfo": info}));
        self.discover.retain(|tx| tx.send(created.clone()).is_ok());
        for tx in self.auto_attach.clone() {
            self.attach(&id, tx);
        }
        id
    }

    fn attach(&mut self, target: &str, tx: mpsc::Sender<String>) {
        self.next_session += 1;
        let session = format!("S{}", self.next_session);
        let url = self
            .pages
            .get(target)
            .map(|p| p.url.clone())
            .unwrap_or_default();
        let _ = tx.send(event(
            None,
            "Target.attachedToTarget",
            json!({
                "sessionId": session,
                "targetInfo": {"targetId": target, "type": "page", "url": url},
                "waitingForDebugger": false,
            }),
        ));
        self.listeners.push(Listener {
            target: target.to_string(),
            session: Some(session),
            tx,
        });
    }

    fn close_page(&mut self, target: &str) {
        self.pages.remove(target);
        let mut kept = Vec::new();
        for l in self.listeners.drain(..) {
            if l.target != target {
                kept.push(l);
            } else if let Some(session) = &l.session {
                let _ = l.tx.send(event(
                    None,
                    "Target.detachedFromTarget",
                    json!({"sessionId": session, "targetId": target}),
                ));
            }
        }
        self.listeners = kept;
        let destroyed = event(None, "Target.targetDestroyed", json!({"targetId": target}));
        self.discover
            .retain(|tx| tx.send(destroyed.clone()).is_ok());
    }

    fn broadcast(&mut self, target: &str, method: &str, params: Value) {
        self.listeners.retain(|l| {
            l.target != target
                || l.tx
                    .send(event(l.session.as_deref(), method, params.clone()))
                    .is_ok()
        });
    }
}

fn event(session: Option<&str>, method: &str, params: Value) -> String {
    let mut event = json!({"method": method, "params": params});
    if let Some(session) = session {
        event["sessionId"] = json!(session);
    }
    event.to_string()
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
    let profile_dir = std::path::Path::new(profile);
    std::fs::write(profile_dir.join("fake.pid"), std::process::id().to_string()).expect("pid");
    if let Some(ms) = std::env::var("CEDIAN_FAKE_BROWSER_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        std::thread::sleep(Duration::from_millis(ms));
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::fs::write(
        std::path::Path::new(profile).join("DevToolsActivePort"),
        format!("{port}\n/devtools/browser/fake\n"),
    )
    .expect("DevToolsActivePort");
    let browser = Arc::new(Mutex::new(Browser {
        profile: profile_dir.to_path_buf(),
        ..Browser::default()
    }));
    browser.lock().open_page("about:blank");
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let browser = browser.clone();
        std::thread::spawn(move || {
            let _ = connection(stream, port, &browser);
        });
    }
    0
}

fn connection(stream: TcpStream, port: u16, browser: &Arc<Mutex<Browser>>) -> Result<(), String> {
    let mut buf = [0u8; 4096];
    let n = stream.peek(&mut buf).map_err(|e| e.to_string())?;
    let head = String::from_utf8_lossy(&buf[..n]).to_string();
    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
    if head.to_lowercase().contains("upgrade: websocket") {
        let target = path.strip_prefix("/devtools/page/").map(str::to_string);
        return websocket(stream, target, browser);
    }
    let mut stream = stream;
    stream.read(&mut buf).map_err(|e| e.to_string())?;
    let ws = format!("ws://127.0.0.1:{port}");
    let body = match path.as_str() {
        "/json/version" => json!({
            "Browser": "FakeChromium/1",
            "webSocketDebuggerUrl": format!("{ws}/devtools/browser/fake"),
        }),
        "/json/list" | "/json" => Value::Array(
            browser
                .lock()
                .pages
                .iter()
                .map(|(id, page)| {
                    json!({
                        "id": id,
                        "type": "page",
                        "url": page.url,
                        "webSocketDebuggerUrl": format!("{ws}/devtools/page/{id}"),
                    })
                })
                .collect(),
        ),
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

/// A page socket (`target` set) or the browser socket.
fn websocket(
    stream: TcpStream,
    target: Option<String>,
    browser: &Arc<Mutex<Browser>>,
) -> Result<(), String> {
    let mut socket = tungstenite::accept(stream).map_err(|e| e.to_string())?;
    socket
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(5)))
        .map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::channel();
    if let Some(target) = &target {
        browser.lock().listeners.push(Listener {
            target: target.clone(),
            session: None,
            tx: tx.clone(),
        });
    }
    loop {
        match socket.read() {
            Ok(Message::Text(text)) => {
                let request: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
                let session = request["sessionId"].as_str().map(str::to_string);
                let page = match &session {
                    Some(session) => browser
                        .lock()
                        .listeners
                        .iter()
                        .find(|l| l.session.as_deref() == Some(session))
                        .map(|l| l.target.clone()),
                    None => target.clone(),
                };
                let result = match page {
                    Some(page) => answer_page(&request, &page, browser),
                    None => Some(answer_browser(&request, browser, &tx)),
                };
                let Some(result) = result else { continue };
                let mut reply = json!({"id": request["id"], "result": result});
                if let Some(session) = session {
                    reply["sessionId"] = json!(session);
                }
                socket
                    .send(Message::Text(reply.to_string().into()))
                    .map_err(|e| e.to_string())?;
                if request["method"] == "Browser.close" {
                    let _ = socket.flush();
                    std::process::exit(0);
                }
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

fn answer_browser(
    request: &Value,
    browser: &Arc<Mutex<Browser>>,
    tx: &mpsc::Sender<String>,
) -> Value {
    let params = &request["params"];
    let mut browser = browser.lock();
    match request["method"].as_str().unwrap_or_default() {
        "Target.setDiscoverTargets" => {
            browser.discover.push(tx.clone());
            json!({})
        }
        "Target.setAutoAttach" => {
            browser.auto_attach.push(tx.clone());
            let pages: Vec<String> = browser.pages.keys().cloned().collect();
            for page in pages {
                browser.attach(&page, tx.clone());
            }
            json!({})
        }
        "Target.createTarget" => {
            let id = browser.open_page(params["url"].as_str().unwrap_or("about:blank"));
            json!({"targetId": id})
        }
        "Browser.close" => {
            let _ = std::fs::write(browser.profile.join("closed-by-cdp"), "");
            json!({})
        }
        "Target.closeTarget" => {
            browser.close_page(params["targetId"].as_str().unwrap_or_default());
            json!({"success": true})
        }
        _ => json!({}),
    }
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
        * 1000.0
}

/// The listener's report of trusted `kind` input, to cedian's binding.
fn report_input(browser: &mut Browser, target: &str, kind: &str) {
    let Some((name, _)) = browser.bindings.last().cloned() else {
        return;
    };
    browser.broadcast(
        target,
        "Runtime.bindingCalled",
        json!({"name": name, "payload": json!({"type": kind, "time": now_ms()}).to_string()}),
    );
}

/// The trusted input events a CDP `Input.*` command makes in the page.
fn input_events(method: &str, params: &Value) -> Vec<&'static str> {
    match (method, params["type"].as_str().unwrap_or_default()) {
        ("Input.dispatchKeyEvent", "keyDown" | "rawKeyDown") => vec!["keydown"],
        ("Input.dispatchMouseEvent", "mousePressed") => vec!["pointerdown"],
        ("Input.dispatchMouseEvent", "mouseWheel") => vec!["wheel"],
        ("Input.dispatchTouchEvent", "touchStart") => {
            let points = params["touchPoints"].as_array().map_or(1, Vec::len);
            vec!["pointerdown"; points]
        }
        _ => vec![],
    }
}

fn answer_page(request: &Value, target: &str, browser: &Arc<Mutex<Browser>>) -> Option<Value> {
    let params = &request["params"];
    let mut browser = browser.lock();
    let Some(page) = browser.pages.get(target) else {
        return Some(json!({}));
    };
    let (url_now, frame) = (page.url.clone(), page.frame.clone());
    Some(match request["method"].as_str().unwrap_or_default() {
        "Page.navigate" => {
            let url = params["url"].as_str().unwrap_or_default().to_string();
            if let Some(page) = browser.pages.get_mut(target) {
                page.url = url.clone();
            }
            browser.request += 1;
            let id = browser.request.to_string();
            browser.broadcast(
                target,
                "Network.requestWillBeSent",
                json!({"requestId": id, "request": {"method": "GET", "url": url}}),
            );
            browser.broadcast(
                target,
                "Page.frameNavigated",
                json!({"frame": {"id": frame, "url": url}}),
            );
            browser.broadcast(
                target,
                "Network.responseReceived",
                json!({"requestId": id, "response": {"url": url, "status": 200}}),
            );
            browser.broadcast(
                target,
                "Runtime.consoleAPICalled",
                json!({"type": "log", "args": [{"type": "string", "value": format!("loaded {url}")}]}),
            );
            json!({"frameId": frame})
        }
        "Fake.childNavigate" => {
            browser.broadcast(
                target,
                "Page.frameNavigated",
                json!({"frame": {"id": format!("{frame}-child"), "parentId": frame, "url": params["url"]}}),
            );
            json!({})
        }
        "Fake.pushState" => {
            let url = params["url"].as_str().unwrap_or_default().to_string();
            if let Some(page) = browser.pages.get_mut(target) {
                page.url = url.clone();
            }
            browser.broadcast(
                target,
                "Page.navigatedWithinDocument",
                json!({"frameId": frame, "url": url}),
            );
            json!({})
        }
        "Page.getFrameTree" => json!({"frameTree": {"frame": {"id": frame, "url": url_now}}}),
        "Fake.navigateMidCapture" => {
            if let Some(page) = browser.pages.get_mut(target) {
                page.navigate_mid_capture = params["times"].as_u64().unwrap_or(1);
            }
            json!({})
        }
        "Page.captureScreenshot" => {
            if let Some(page) = browser.pages.get_mut(target)
                && page.navigate_mid_capture > 0
            {
                page.navigate_mid_capture -= 1;
                let url = format!("https://moved.test/{}", page.navigate_mid_capture);
                page.url = url.clone();
                browser.broadcast(
                    target,
                    "Page.frameNavigated",
                    json!({"frame": {"id": frame, "url": url}}),
                );
            }
            json!({"data": base64::engine::general_purpose::STANDARD.encode(PNG)})
        }
        "Runtime.evaluate" => {
            let html = format!("<html><body>{url_now}</body></html>");
            json!({"result": {"type": "object", "value": {"url": url_now, "title": "fake", "html": html}}})
        }
        "Runtime.addBinding" => {
            let name = params["name"].as_str().unwrap_or_default().to_string();
            let world = params["executionContextName"].as_str().map(str::to_string);
            browser.bindings.push((name, world));
            json!({})
        }
        "Page.createIsolatedWorld" => json!({"executionContextId": 7}),
        "Fake.personInput" => {
            let kind = params["type"].as_str().unwrap_or("pointerdown");
            report_input(&mut browser, target, kind);
            json!({})
        }
        "Fake.pageScriptCalls" => {
            let name = params["name"].as_str().unwrap_or_default();
            if browser
                .bindings
                .iter()
                .any(|(n, world)| n == name && world.is_none())
            {
                let mut payload = params["payload"].clone();
                payload["time"] = json!(now_ms());
                browser.broadcast(
                    target,
                    "Runtime.bindingCalled",
                    json!({"name": name, "payload": payload.to_string()}),
                );
            }
            json!({})
        }
        method if method.starts_with("Input.") => {
            if params["fakeNoReply"] == true {
                return None;
            }
            for kind in input_events(method, params) {
                report_input(&mut browser, target, kind);
            }
            json!({})
        }
        _ => json!({}),
    })
}

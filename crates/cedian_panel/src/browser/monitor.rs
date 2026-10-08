//! cedian's own CDP line to the shared page: the frame sequence, console
//! and network lines, the person's input, and captures.

use super::cdp::{CdpClient, Notification};
use super::chromium::http_get;
use super::{Capture, Shared};
use base64::Engine as _;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Weak};
use std::time::Duration;

const CALL: Duration = Duration::from_secs(10);
const KEEP_LINES: usize = 200;
const DOM_BYTES: usize = 64 * 1024;
/// The isolated world cedian's input listener and binding live in: the
/// page's own scripts cannot see or call the binding.
const INPUT_WORLD: &str = "cedian";

/// A binding name for this launch, so not even a guess reaches it from
/// another world.
fn binding_name() -> String {
    use rand::{Rng as _, SeedableRng as _};
    format!(
        "cedian{:016x}",
        rand::rngs::StdRng::from_os_rng().random::<u64>()
    )
}

/// Reports trusted (person- or CDP-made) input to `binding`, at the wall
/// clock the endpoint also stamps OMP's input with: `e.timeStamp` is
/// monotonic and drifts from it after a sleep or a clock step.
fn input_listener(binding: &str) -> String {
    format!(
        "(() => {{ if (window.__cedianInput) return; window.__cedianInput = 1; \
for (const t of ['pointerdown', 'keydown', 'wheel']) addEventListener(t, e => {{ \
if (e.isTrusted && window['{binding}']) window['{binding}'](JSON.stringify({{type: t, \
time: Date.now()}})); }}, true); }})()"
    )
}

const KEEP_REQUESTS: usize = 500;
/// How long to wait for the first page after the debugging port opens.
const FIRST_PAGE: Duration = Duration::from_secs(5);
/// How often the line checks that Chromium is still running.
const EXIT_CHECK: Duration = Duration::from_millis(500);

pub(super) type CaptureRequests = mpsc::Sender<mpsc::Sender<Result<Capture, String>>>;

/// One page, followed over its flat session.
#[derive(Default)]
struct Tab {
    frame_id: String,
    url: String,
    console: Vec<String>,
    network: Vec<String>,
    requests: HashMap<String, String>,
}

/// Every page in the browser, by session. Captures come from the active
/// one: the page that last had a main-frame navigation or trusted input,
/// which is the page the agent last drove or the person last used. When it
/// closes, the most recently attached open page takes over.
#[derive(Default)]
struct Tabs {
    by_session: Vec<(String, Tab)>,
    active: Option<String>,
    binding: String,
}

impl Tabs {
    fn get(&mut self, session: &str) -> Option<&mut Tab> {
        self.by_session
            .iter_mut()
            .find(|(s, _)| s == session)
            .map(|(_, t)| t)
    }
}

/// Attach at the browser target and follow every page.
pub(super) fn watch(port: u16, shared: Arc<Shared>) -> Result<CaptureRequests, String> {
    let mut client = CdpClient::connect(&browser_socket(port)?)?;
    client.call("Target.setDiscoverTargets", json!({"discover": true}), CALL)?;
    client.call(
        "Target.setAutoAttach",
        json!({"autoAttach": true, "flatten": true, "waitForDebuggerOnStart": false}),
        CALL,
    )?;
    let mut tabs = Tabs {
        binding: binding_name(),
        ..Tabs::default()
    };
    let deadline = std::time::Instant::now() + FIRST_PAGE;
    while tabs.active.is_none() && std::time::Instant::now() < deadline {
        client.poll()?;
        let notifications = client.drain();
        handle(&mut client, &shared, &mut tabs, notifications);
    }
    shared.state.lock().seq += 1;
    mirror(&shared, &tabs);
    let (tx, rx) = mpsc::channel();
    let weak = Arc::downgrade(&shared);
    drop(shared);
    std::thread::spawn(move || run(client, tabs, rx, weak));
    Ok(tx)
}

fn run(
    mut client: CdpClient,
    mut tabs: Tabs,
    requests: mpsc::Receiver<mpsc::Sender<Result<Capture, String>>>,
    shared: Weak<Shared>,
) {
    let mut checked = std::time::Instant::now();
    loop {
        let Some(shared) = shared.upgrade() else {
            return;
        };
        if shared.closed() {
            return;
        }
        let mut result = match requests.try_recv() {
            Ok(reply) => {
                let capture = capture(&mut client, &shared, &mut tabs);
                let _ = reply.send(capture);
                Ok(())
            }
            Err(mpsc::TryRecvError::Empty) => client.poll(),
            Err(mpsc::TryRecvError::Disconnected) => return,
        };
        if result.is_ok() && checked.elapsed() >= EXIT_CHECK {
            checked = std::time::Instant::now();
            if shared
                .running
                .lock()
                .as_mut()
                .is_some_and(|r| r.chromium.exited())
            {
                result = Err("Chromium exited".to_string());
            }
        }
        if let Err(e) = result {
            let mut state = shared.state.lock();
            state.running = false;
            state.error = Some(format!("the browser connection ended: {e}"));
            drop(state);
            shared.running.lock().take();
            (shared.changed)();
            return;
        }
        let notifications = client.drain();
        if !notifications.is_empty() {
            handle(&mut client, &shared, &mut tabs, notifications);
        }
    }
}

/// Start following the page on `session`. A setup call that fails is
/// logged, not fatal: the page is still followed, so its navigations count.
fn follow(client: &mut CdpClient, session: &str, binding: &str) -> Tab {
    let listener = input_listener(binding);
    let mut call = |method: &str, params: Value| {
        client
            .call_in(Some(session), method, params, CALL)
            .inspect_err(|e| log::debug!("browser: {method} on {session}: {e}"))
            .ok()
    };
    call("Page.enable", json!({}));
    call("Runtime.enable", json!({}));
    call("Network.enable", json!({}));
    call(
        "Runtime.addBinding",
        json!({"name": binding, "executionContextName": INPUT_WORLD}),
    );
    call(
        "Page.addScriptToEvaluateOnNewDocument",
        json!({"source": listener, "worldName": INPUT_WORLD}),
    );
    let tree = call("Page.getFrameTree", json!({})).unwrap_or_default();
    let frame_id = text(&tree, "/frameTree/frame/id");
    if !frame_id.is_empty()
        && let Some(world) = call(
            "Page.createIsolatedWorld",
            json!({"frameId": frame_id, "worldName": INPUT_WORLD}),
        )
    {
        call(
            "Runtime.evaluate",
            json!({"expression": listener, "contextId": world["executionContextId"]}),
        );
    }
    Tab {
        frame_id,
        url: text(&tree, "/frameTree/frame/url"),
        ..Tab::default()
    }
}

/// Apply notifications, following pages as they attach; attaching may
/// bring more notifications, applied in turn.
fn handle(
    client: &mut CdpClient,
    shared: &Shared,
    tabs: &mut Tabs,
    mut notifications: Vec<Notification>,
) {
    while !notifications.is_empty() {
        for notification in notifications {
            match notification.method.as_str() {
                "Target.attachedToTarget"
                    if text(&notification.params, "/targetInfo/type") == "page" =>
                {
                    let session = text(&notification.params, "/sessionId");
                    let tab = follow(client, &session, &tabs.binding);
                    tabs.by_session.push((session.clone(), tab));
                    tabs.active.get_or_insert(session);
                }
                "Target.detachedFromTarget" => {
                    let session = text(&notification.params, "/sessionId");
                    tabs.by_session.retain(|(s, _)| *s != session);
                    if tabs.active.as_deref() == Some(session.as_str()) {
                        tabs.active = tabs.by_session.last().map(|(s, _)| s.clone());
                    }
                }
                _ => apply(shared, tabs, notification),
            }
        }
        notifications = client.drain();
    }
    mirror(shared, tabs);
    (shared.changed)();
}

/// The active page's frame, console and network into the panel's state.
fn mirror(shared: &Shared, tabs: &Tabs) {
    let tab = tabs
        .active
        .as_ref()
        .and_then(|a| tabs.by_session.iter().find(|(s, _)| s == a))
        .map(|(_, t)| t);
    let mut state = shared.state.lock();
    match tab {
        Some(tab) => {
            state.frame_id = tab.frame_id.clone();
            state.url = tab.url.clone();
            state.console = tab.console.clone();
            state.network = tab.network.clone();
        }
        None => {
            state.frame_id.clear();
            state.url.clear();
            state.console.clear();
            state.network.clear();
        }
    }
}

/// How many times a capture is retried when the page navigates under it.
const CAPTURE_TRIES: usize = 3;

/// A capture whose screenshot and DOM come from one frame: the sequence is
/// read before the screenshot and again after the DOM read, and a capture
/// the page navigated under is retried, then marked born stale.
fn capture(client: &mut CdpClient, shared: &Shared, tabs: &mut Tabs) -> Result<Capture, String> {
    let mut tries = 0;
    loop {
        tries += 1;
        let notifications = client.drain();
        handle(client, shared, tabs, notifications);
        let before = shared.state.lock().seq;
        let mut capture = capture_once(client, shared, tabs)?;
        if capture.seq == before {
            return Ok(capture);
        }
        if tries >= CAPTURE_TRIES {
            capture.moved = true;
            return Ok(capture);
        }
    }
}

fn capture_once(
    client: &mut CdpClient,
    shared: &Shared,
    tabs: &mut Tabs,
) -> Result<Capture, String> {
    let session = tabs.active.clone().ok_or("the browser has no open page")?;
    let shot = client.call_in(
        Some(&session),
        "Page.captureScreenshot",
        json!({"format": "png", "fromSurface": true}),
        CALL,
    )?;
    let png = base64::engine::general_purpose::STANDARD
        .decode(shot.get("data").and_then(Value::as_str).unwrap_or_default())
        .map_err(|e| format!("screenshot: {e}"))?;
    let page = client.call_in(
        Some(&session),
        "Runtime.evaluate",
        json!({
            "expression": "({url: location.href, title: document.title, html: document.documentElement.outerHTML})",
            "returnByValue": true,
        }),
        CALL,
    )?;
    let notifications = client.drain();
    handle(client, shared, tabs, notifications);
    let state = shared.state.lock();
    let mut dom = text(&page, "/result/value/html");
    if dom.len() > DOM_BYTES {
        let mut end = DOM_BYTES;
        while !dom.is_char_boundary(end) {
            end -= 1;
        }
        dom.truncate(end);
    }
    Ok(Capture {
        frame_id: state.frame_id.clone(),
        seq: state.seq,
        png: Arc::new(png),
        url: text(&page, "/result/value/url"),
        title: text(&page, "/result/value/title"),
        dom,
        console: state.console.clone(),
        network: state.network.clone(),
        moved: false,
    })
}

/// One page notification. Main-frame navigations in any page advance the
/// one browser-wide sequence: evidence is bound to the browser, not a tab,
/// so a navigation anywhere the agent could have looked makes earlier
/// captures stale.
fn apply(shared: &Shared, tabs: &mut Tabs, notification: Notification) {
    let Notification {
        method,
        params,
        session,
    } = notification;
    let Some(session) = session else { return };
    let binding = tabs.binding.clone();
    let Some(tab) = tabs.get(&session) else {
        return;
    };
    match method.as_str() {
        "Page.frameNavigated" if params.pointer("/frame/parentId").is_none() => {
            tab.frame_id = text(&params, "/frame/id");
            tab.url = text(&params, "/frame/url");
            tab.console.clear();
            tab.network.clear();
            shared.state.lock().seq += 1;
            tabs.active = Some(session);
        }
        "Runtime.consoleAPICalled" => {
            let args: Vec<String> = params["args"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|a| match a.get("value") {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => text(a, "/description"),
                })
                .collect();
            push(
                &mut tab.console,
                format!("{}: {}", text(&params, "/type"), args.join(" ")),
            );
        }
        "Runtime.exceptionThrown" => {
            let detail = params
                .pointer("/exceptionDetails/exception/description")
                .or_else(|| params.pointer("/exceptionDetails/text"))
                .and_then(Value::as_str)
                .unwrap_or("exception");
            push(&mut tab.console, format!("error: {detail}"));
        }
        "Network.requestWillBeSent" => {
            // Not cleared on navigation: the document's own request is sent
            // before its frame navigates.
            if tab.requests.len() >= KEEP_REQUESTS {
                tab.requests.clear();
            }
            tab.requests.insert(
                text(&params, "/requestId"),
                format!(
                    "{} {}",
                    text(&params, "/request/method"),
                    text(&params, "/request/url")
                ),
            );
        }
        "Network.responseReceived" => {
            let request = tab
                .requests
                .remove(&text(&params, "/requestId"))
                .unwrap_or_else(|| text(&params, "/response/url"));
            let status = params
                .pointer("/response/status")
                .map(Value::to_string)
                .unwrap_or_default();
            push(&mut tab.network, format!("{status} {request}"));
        }
        "Network.loadingFailed" => {
            let request = tab
                .requests
                .remove(&text(&params, "/requestId"))
                .unwrap_or_default();
            push(
                &mut tab.network,
                format!("failed ({}) {request}", text(&params, "/errorText")),
            );
        }
        "Runtime.bindingCalled" if text(&params, "/name") == binding => {
            let payload =
                serde_json::from_str::<Value>(&text(&params, "/payload")).unwrap_or_default();
            tabs.active = Some(session);
            shared.person_input(
                &text(&payload, "/type"),
                payload.get("time").and_then(Value::as_f64),
            );
        }
        _ => {}
    }
}

fn push(lines: &mut Vec<String>, line: String) {
    if lines.len() >= KEEP_LINES {
        lines.remove(0);
    }
    lines.push(line);
}

fn text(value: &Value, pointer: &str) -> String {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The browser target's WebSocket.
fn browser_socket(port: u16) -> Result<String, String> {
    let version: Value = serde_json::from_str(&http_get(port, "/json/version")?)
        .map_err(|e| format!("browser version: {e}"))?;
    version
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "the browser has no debugger URL".to_string())
}

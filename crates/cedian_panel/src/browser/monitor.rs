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
const INPUT_BINDING: &str = "cedianInput";
/// Reports trusted (person- or CDP-made) input to cedian's binding.
const INPUT_LISTENER: &str = "(() => { if (window.__cedianInput) return; window.__cedianInput = 1; \
for (const t of ['pointerdown', 'keydown', 'wheel']) addEventListener(t, e => { \
if (e.isTrusted && window.cedianInput) window.cedianInput(JSON.stringify({type: t, \
time: performance.timeOrigin + e.timeStamp})); }, true); })()";

pub(super) type CaptureRequests = mpsc::Sender<mpsc::Sender<Result<Capture, String>>>;

pub(super) fn watch(port: u16, shared: Arc<Shared>) -> Result<CaptureRequests, String> {
    let mut client = CdpClient::connect(&page_socket(port)?)?;
    for (method, params) in [
        ("Page.enable", json!({})),
        ("Runtime.enable", json!({})),
        ("Network.enable", json!({})),
        ("Runtime.addBinding", json!({"name": INPUT_BINDING})),
        (
            "Page.addScriptToEvaluateOnNewDocument",
            json!({"source": INPUT_LISTENER}),
        ),
        ("Runtime.evaluate", json!({"expression": INPUT_LISTENER})),
    ] {
        client.call(method, params, CALL)?;
    }
    let tree = client.call("Page.getFrameTree", json!({}), CALL)?;
    {
        let mut state = shared.state.lock();
        state.frame_id = text(&tree, "/frameTree/frame/id");
        state.url = text(&tree, "/frameTree/frame/url");
        state.seq += 1;
    }
    let (tx, rx) = mpsc::channel();
    let weak = Arc::downgrade(&shared);
    drop(shared);
    std::thread::spawn(move || run(client, rx, weak));
    Ok(tx)
}

fn run(
    mut client: CdpClient,
    requests: mpsc::Receiver<mpsc::Sender<Result<Capture, String>>>,
    shared: Weak<Shared>,
) {
    let mut requests_by_id: HashMap<String, String> = HashMap::new();
    loop {
        let Some(shared) = shared.upgrade() else {
            return;
        };
        if shared.closed() {
            return;
        }
        let result = match requests.try_recv() {
            Ok(reply) => {
                let capture = capture(&mut client, &shared, &mut requests_by_id);
                let _ = reply.send(capture);
                Ok(())
            }
            Err(mpsc::TryRecvError::Empty) => client.poll(),
            Err(mpsc::TryRecvError::Disconnected) => return,
        };
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
            apply(&shared, notifications, &mut requests_by_id);
        }
    }
}

fn capture(
    client: &mut CdpClient,
    shared: &Shared,
    requests: &mut HashMap<String, String>,
) -> Result<Capture, String> {
    let shot = client.call(
        "Page.captureScreenshot",
        json!({"format": "png", "fromSurface": true}),
        CALL,
    )?;
    let png = base64::engine::general_purpose::STANDARD
        .decode(shot.get("data").and_then(Value::as_str).unwrap_or_default())
        .map_err(|e| format!("screenshot: {e}"))?;
    let page = client.call(
        "Runtime.evaluate",
        json!({
            "expression": "({url: location.href, title: document.title, html: document.documentElement.outerHTML})",
            "returnByValue": true,
        }),
        CALL,
    )?;
    apply(shared, client.drain(), requests);
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
    })
}

fn apply(
    shared: &Shared,
    notifications: Vec<Notification>,
    requests: &mut HashMap<String, String>,
) {
    let mut person = None;
    {
        let mut state = shared.state.lock();
        for Notification { method, params } in notifications {
            match method.as_str() {
                "Page.frameNavigated" if params.pointer("/frame/parentId").is_none() => {
                    state.seq += 1;
                    state.frame_id = text(&params, "/frame/id");
                    state.url = text(&params, "/frame/url");
                    state.console.clear();
                    state.network.clear();
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
                        &mut state.console,
                        format!("{}: {}", text(&params, "/type"), args.join(" ")),
                    );
                }
                "Runtime.exceptionThrown" => {
                    let detail = params
                        .pointer("/exceptionDetails/exception/description")
                        .or_else(|| params.pointer("/exceptionDetails/text"))
                        .and_then(Value::as_str)
                        .unwrap_or("exception");
                    push(&mut state.console, format!("error: {detail}"));
                }
                "Network.requestWillBeSent" => {
                    requests.insert(
                        text(&params, "/requestId"),
                        format!(
                            "{} {}",
                            text(&params, "/request/method"),
                            text(&params, "/request/url")
                        ),
                    );
                }
                "Network.responseReceived" => {
                    let request = requests
                        .remove(&text(&params, "/requestId"))
                        .unwrap_or_else(|| text(&params, "/response/url"));
                    let status = params
                        .pointer("/response/status")
                        .map(Value::to_string)
                        .unwrap_or_default();
                    push(&mut state.network, format!("{status} {request}"));
                }
                "Network.loadingFailed" => {
                    let request = requests
                        .remove(&text(&params, "/requestId"))
                        .unwrap_or_default();
                    push(
                        &mut state.network,
                        format!("failed ({}) {request}", text(&params, "/errorText")),
                    );
                }
                "Runtime.bindingCalled" if text(&params, "/name") == INPUT_BINDING => {
                    let at = serde_json::from_str::<Value>(&text(&params, "/payload"))
                        .ok()
                        .and_then(|p| p.get("time")?.as_f64());
                    person = Some(at);
                }
                _ => {}
            }
        }
    }
    if let Some(at) = person {
        shared.person_input(at);
    }
    (shared.changed)();
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

/// The first page target's WebSocket.
fn page_socket(port: u16) -> Result<String, String> {
    let list: Value = serde_json::from_str(&http_get(port, "/json/list")?)
        .map_err(|e| format!("target list: {e}"))?;
    list.as_array()
        .into_iter()
        .flatten()
        .find(|t| t.get("type").and_then(Value::as_str) == Some("page"))
        .and_then(|t| t.get("webSocketDebuggerUrl")?.as_str())
        .map(str::to_string)
        .ok_or_else(|| "the browser has no page".to_string())
}

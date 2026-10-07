//! Sync CDP client: id-multiplexed `call` + event pump (S4 Task 2).
//!
//! Blocking API like `cedian_lsp`/`cedian_dap`: one in-flight call at a
//! time, a 100ms socket-read-timeout retry loop to the call deadline, and
//! `Page.frameNavigated` / `Page.loadEventFired` notifications pumped into
//! an event queue while waiting. Unknown notification methods are ignored.

use std::io::ErrorKind;
use std::time::{Duration, Instant};

use serde_json::Value;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// Underlying transport: plain TCP (no TLS features enabled) or TLS.
type WsStream = WebSocket<MaybeTlsStream<std::net::TcpStream>>;

/// CDP notifications the client pumps into its event queue.
#[derive(Debug, PartialEq, Eq)]
pub enum CdpEvent {
    /// `Page.frameNavigated` — carries the navigated frame's id.
    FrameNavigated {
        /// CDP frame id (`params.frame.id`).
        frame_id: String,
    },
    /// `Page.loadEventFired`.
    LoadEventFired,
}

/// Failures connecting, speaking, or waiting on CDP.
#[derive(Debug)]
pub enum CdpError {
    /// `tungstenite::connect` failed (message carries the handshake detail).
    Connect(String),
    /// WebSocket I/O or malformed CDP JSON (message carries detail).
    Io(String),
    /// No matching response arrived before the call deadline.
    Timeout(String),
    /// CDP answered with an `error` object.
    Rpc {
        /// CDP error code.
        code: i64,
        /// CDP error message.
        message: String,
    },
}

impl std::fmt::Display for CdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CdpError::Connect(msg) => write!(f, "cdp connect: {msg}"),
            CdpError::Io(msg) => write!(f, "cdp io: {msg}"),
            CdpError::Timeout(msg) => write!(f, "cdp timeout: {msg}"),
            CdpError::Rpc { code, message } => write!(f, "cdp rpc error {code}: {message}"),
        }
    }
}

impl std::error::Error for CdpError {}

/// Owned CDP connection: id-multiplexed calls plus a pumped event queue.
pub struct CdpClient {
    socket: WsStream,
    next_id: u64,
    events: Vec<CdpEvent>,
}

impl CdpClient {
    /// Open a CDP WebSocket connection to `url`.
    pub fn connect(url: &str) -> Result<CdpClient, CdpError> {
        let (socket, _) =
            tungstenite::connect(url).map_err(|e| CdpError::Connect(e.to_string()))?;
        Ok(CdpClient {
            socket,
            next_id: 0,
            events: Vec::new(),
        })
    }

    /// Send `{"id","method","params"}` and wait up to `timeout` for the
    /// response with the matching `id`.
    ///
    /// While waiting, `Page.frameNavigated` / `Page.loadEventFired`
    /// notifications are pumped into the event queue (see `drain_events`);
    /// unknown methods are ignored.
    pub fn call(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CdpError> {
        self.call_session(method, params, None, timeout)
    }

    /// Send `{"id","method","params"}` and wait up to `timeout` for the
    /// response with the matching `id`. With `session_id`, the call is
    /// scoped to an attached target (flattened sessions); matching
    /// `sessionId` notifications are pumped (see `drain_events`).
    pub fn call_session(
        &mut self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
        timeout: Duration,
    ) -> Result<Value, CdpError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut request = serde_json::json!({"id": id, "method": method, "params": params});
        if let Some(sid) = session_id {
            request["sessionId"] = Value::String(sid.to_string());
        }
        let text = serde_json::to_string(&request).map_err(|e| CdpError::Io(e.to_string()))?;
        set_read_timeout(&mut self.socket, Some(Duration::from_millis(100)))?;
        self.socket
            .send(Message::Text(text.into()))
            .map_err(|e| CdpError::Io(e.to_string()))?;
        let deadline = Instant::now() + timeout;
        loop {
            match self.socket.read() {
                Ok(Message::Text(body)) => {
                    let msg: Value =
                        serde_json::from_str(&body).map_err(|e| CdpError::Io(e.to_string()))?;
                    if let Some(rid) = msg.get("id").and_then(Value::as_u64) {
                        if rid == id {
                            if let Some(error) = msg.get("error") {
                                let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
                                let message = error
                                    .get("message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown error")
                                    .to_string();
                                return Err(CdpError::Rpc { code, message });
                            }
                            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
                        }
                        // A response for another id: not ours (single-flight
                        // client); ignore it.
                    } else if let Some(name) = msg.get("method").and_then(Value::as_str) {
                        // Flattened sessions tag notifications with sessionId:
                        // only pump events for our call's session (None =
                        // browser-level, pumps only untagged traffic).
                        let tag = msg.get("sessionId").and_then(Value::as_str);
                        if tag != session_id {
                            continue;
                        }
                        match name {
                            "Page.frameNavigated" => {
                                let frame_id = msg
                                    .pointer("/params/frame/id")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string();
                                self.events.push(CdpEvent::FrameNavigated { frame_id });
                            }
                            "Page.loadEventFired" => {
                                self.events.push(CdpEvent::LoadEventFired);
                            }
                            // Tolerance rule: ignore unknown methods.
                            _ => {}
                        }
                    }
                }
                // Non-text frames (binary/ping/close): not CDP traffic.
                Ok(_) => {}
                Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                    // No frame within the 100ms slice; fall through to the
                    // deadline check and keep waiting.
                }
                Err(e) => return Err(CdpError::Io(e.to_string())),
            }
            if Instant::now() >= deadline {
                return Err(CdpError::Timeout(format!(
                    "{method} timed out after {timeout:?}"
                )));
            }
        }
    }

    /// Take all pumped events, oldest first.
    pub fn drain_events(&mut self) -> Vec<CdpEvent> {
        std::mem::take(&mut self.events)
    }
}

/// Set the socket read timeout used by the `call` retry loop.
fn set_read_timeout(socket: &mut WsStream, timeout: Option<Duration>) -> Result<(), CdpError> {
    match socket.get_mut() {
        MaybeTlsStream::Plain(tcp) => tcp
            .set_read_timeout(timeout)
            .map_err(|e| CdpError::Io(e.to_string())),
        // TLS variants are compiled out (no TLS features enabled).
        _ => Err(CdpError::Connect(
            "non-plain websocket stream unsupported".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(5);

    #[test]
    fn call_matches_response_id_and_pumps_events() {
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut ws = tungstenite::accept(server.accept().unwrap().0).unwrap();
            // Push an event first, then answer the call with interleaved traffic.
            ws.send(tungstenite::Message::Text(
                r#"{"method":"Page.loadEventFired","params":{}}"#.into(),
            ))
            .unwrap();
            let msg = ws.read().unwrap().into_text().unwrap();
            let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
            let id = v["id"].as_u64().unwrap();
            ws.send(tungstenite::Message::Text(
                format!(r#"{{"id":{id},"result":{{"frameId":"F1"}}}}"#).into(),
            ))
            .unwrap();
        });
        let mut client = CdpClient::connect(&format!("ws://{addr}")).unwrap();
        let result = client
            .call(
                "Page.navigate",
                serde_json::json!({"url": "about:blank"}),
                TIMEOUT,
            )
            .unwrap();
        assert_eq!(result["frameId"], "F1");
        assert!(matches!(
            client.drain_events()[..],
            [CdpEvent::LoadEventFired]
        ));
    }
}

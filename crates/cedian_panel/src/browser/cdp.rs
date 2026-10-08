//! A blocking CDP client over one WebSocket: id-matched calls, with every
//! notification that arrives meanwhile kept for [`CdpClient::drain`].

use serde_json::{Value, json};
use std::io::ErrorKind;
use std::net::TcpStream;
use std::time::{Duration, Instant};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

/// One CDP notification.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub method: String,
    pub params: Value,
    /// The flat session it came from; `None` for the browser target.
    pub session: Option<String>,
}

pub struct CdpClient {
    socket: Socket,
    next_id: u64,
    pending: Vec<Notification>,
}

impl CdpClient {
    pub fn connect(url: &str) -> Result<Self, String> {
        let (socket, _) = tungstenite::connect(url).map_err(|e| format!("cdp connect: {e}"))?;
        Self::from_socket(socket)
    }

    /// [`Self::connect`] to a plain `ws://` URL, giving up after `limit`.
    pub fn connect_within(url: &str, limit: Duration) -> Result<Self, String> {
        if limit.is_zero() {
            return Err("cdp connect: out of time".to_string());
        }
        let deadline = Instant::now() + limit;
        let uri: tungstenite::http::Uri = url.parse().map_err(|e| format!("cdp connect: {e}"))?;
        let addr = std::net::ToSocketAddrs::to_socket_addrs(&(
            uri.host().unwrap_or_default(),
            uri.port_u16().unwrap_or(80),
        ))
        .map_err(|e| format!("cdp connect: {e}"))?
        .next()
        .ok_or_else(|| "cdp connect: no address".to_string())?;
        let tcp =
            TcpStream::connect_timeout(&addr, limit).map_err(|e| format!("cdp connect: {e}"))?;
        let left = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1));
        tcp.set_read_timeout(Some(left))
            .and_then(|()| tcp.set_write_timeout(Some(left)))
            .map_err(|e| format!("cdp: {e}"))?;
        let (socket, _) = tungstenite::client(url, MaybeTlsStream::Plain(tcp))
            .map_err(|e| format!("cdp connect: {e}"))?;
        Self::from_socket(socket)
    }

    fn from_socket(socket: Socket) -> Result<Self, String> {
        if let MaybeTlsStream::Plain(tcp) = socket.get_ref() {
            tcp.set_read_timeout(Some(Duration::from_millis(20)))
                .map_err(|e| format!("cdp: {e}"))?;
        }
        Ok(Self {
            socket,
            next_id: 1,
            pending: Vec::new(),
        })
    }

    /// Send `method` to the browser target and wait up to `timeout`.
    pub fn call(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        self.call_in(None, method, params, timeout)
    }

    /// Send `method` on flat `session` (or the browser target) and wait up to
    /// `timeout` for its result.
    pub fn call_in(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let mut request = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            request["sessionId"] = json!(session);
        }
        self.socket
            .send(Message::Text(request.to_string().into()))
            .map_err(|e| format!("cdp send: {e}"))?;
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(message) = self.read()? {
                if message.get("id").and_then(Value::as_u64) == Some(id) {
                    if let Some(error) = message.get("error") {
                        return Err(format!("{method}: {error}"));
                    }
                    return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                }
                self.keep(message);
            }
            if Instant::now() >= deadline {
                return Err(format!("{method} timed out after {timeout:?}"));
            }
        }
    }

    /// Read whatever arrives within one short read slice.
    pub fn poll(&mut self) -> Result<(), String> {
        if let Some(message) = self.read()? {
            self.keep(message);
        }
        Ok(())
    }

    /// The notifications received so far, oldest first.
    pub fn drain(&mut self) -> Vec<Notification> {
        std::mem::take(&mut self.pending)
    }

    fn keep(&mut self, message: Value) {
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            self.pending.push(Notification {
                method: method.to_string(),
                params: message.get("params").cloned().unwrap_or(Value::Null),
                session: message
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
    }

    fn read(&mut self) -> Result<Option<Value>, String> {
        match self.socket.read() {
            Ok(Message::Text(body)) => serde_json::from_str(&body)
                .map(Some)
                .map_err(|e| format!("cdp json: {e}")),
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                Ok(None)
            }
            Err(e) => Err(format!("cdp read: {e}")),
        }
    }
}

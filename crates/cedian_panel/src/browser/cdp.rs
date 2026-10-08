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
}

pub struct CdpClient {
    socket: Socket,
    next_id: u64,
    pending: Vec<Notification>,
}

impl CdpClient {
    pub fn connect(url: &str) -> Result<Self, String> {
        let (socket, _) = tungstenite::connect(url).map_err(|e| format!("cdp connect: {e}"))?;
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

    /// Send `method` and wait up to `timeout` for its result.
    pub fn call(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"id": id, "method": method, "params": params});
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

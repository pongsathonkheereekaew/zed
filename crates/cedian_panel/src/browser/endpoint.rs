//! The endpoint OMP is given: CDP discovery over HTTP and the CDP
//! WebSockets, forwarded to the Chromium it starts on first use.

use super::Shared;
use super::chromium::http_get;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

const SLICE: Duration = Duration::from_millis(5);

pub(super) fn serve(listener: TcpListener, shared: Arc<Shared>) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if shared.closed() {
                break;
            }
            let Ok(stream) = stream else { continue };
            let shared = shared.clone();
            std::thread::spawn(move || {
                if let Err(e) = connection(stream, &shared) {
                    log::debug!("browser endpoint: {e}");
                }
            });
        }
    });
}

fn connection(stream: TcpStream, shared: &Arc<Shared>) -> Result<(), String> {
    let head = peek_head(&stream)?;
    let own = stream.local_addr().map_err(|e| e.to_string())?.port();
    let header = |name: &str| {
        head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
    };
    let path = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_string();
    let upgrade = header("upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    // Chromium refuses a foreign Host (DNS rebinding) and any Origin on its
    // own; this proxy dials it without either, so it has to refuse them here.
    // OMP and Puppeteer send no Origin.
    let host_ok = header("host")
        .is_some_and(|h| h == format!("127.0.0.1:{own}") || h == format!("localhost:{own}"));
    if !host_ok || header("origin").is_some() {
        return refuse(stream, &head, "403 Forbidden");
    }
    if upgrade && path.starts_with("/devtools/") {
        websocket(stream, shared)
    } else if !upgrade && matches!(path.as_str(), "/json/version" | "/json/list" | "/json") {
        discovery(stream, &head, &path, own, shared)
    } else {
        refuse(stream, &head, "404 Not Found")
    }
}

fn refuse(mut stream: TcpStream, head: &str, status: &str) -> Result<(), String> {
    let mut sink = vec![0u8; head.len() + 4];
    stream.read_exact(&mut sink).map_err(|e| e.to_string())?;
    stream
        .write_all(
            format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .map_err(|e| e.to_string())
}

fn peek_head(stream: &TcpStream) -> Result<String, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 8192];
    for _ in 0..500 {
        let n = stream.peek(&mut buf).map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&buf[..n]);
        if let Some((head, _)) = text.split_once("\r\n\r\n") {
            return Ok(head.to_string());
        }
        if n == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err("incomplete request".to_string())
}

/// `GET /json/...`: start the browser, ask it, and point its WebSocket URLs
/// back at this endpoint.
fn discovery(
    mut stream: TcpStream,
    head: &str,
    path: &str,
    own: u16,
    shared: &Arc<Shared>,
) -> Result<(), String> {
    let mut sink = vec![0u8; head.len() + 4];
    stream.read_exact(&mut sink).map_err(|e| e.to_string())?;
    let reply = shared.start().and_then(|port| {
        http_get(port, path).map(|body| {
            body.replace(&format!("127.0.0.1:{port}"), &format!("127.0.0.1:{own}"))
                .replace(&format!("localhost:{port}"), &format!("127.0.0.1:{own}"))
        })
    });
    let (status, body) = match reply {
        Ok(body) => ("200 OK", body),
        Err(e) => ("502 Bad Gateway", e),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json; charset=UTF-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .map_err(|e| e.to_string())
}

/// A CDP WebSocket: forwarded both ways; OMP's side is held while the
/// person has the window.
#[allow(
    clippy::result_large_err,
    reason = "tungstenite's handshake callback returns its own error response"
)]
fn websocket(stream: TcpStream, shared: &Arc<Shared>) -> Result<(), String> {
    let port = shared.start()?;
    let mut path = String::new();
    let mut client = tungstenite::accept_hdr(
        stream,
        |request: &tungstenite::handshake::server::Request, response| {
            path = request.uri().to_string();
            Ok(response)
        },
    )
    .map_err(|e| format!("accept: {e}"))?;
    let (mut upstream, _) = tungstenite::connect(format!("ws://127.0.0.1:{port}{path}"))
        .map_err(|e| format!("upstream: {e}"))?;
    client
        .get_ref()
        .set_read_timeout(Some(SLICE))
        .map_err(|e| e.to_string())?;
    if let MaybeTlsStream::Plain(tcp) = upstream.get_ref() {
        tcp.set_read_timeout(Some(SLICE))
            .map_err(|e| e.to_string())?;
    }
    while !shared.closed() {
        if !shared.held() {
            if let Some(message) = read(&mut client)? {
                if let Message::Text(text) = &message
                    && serde_json::from_str::<serde_json::Value>(text)
                        .ok()
                        .and_then(|v| v.get("method")?.as_str().map(|m| m.starts_with("Input.")))
                        .unwrap_or(false)
                {
                    shared.agent_input();
                }
                upstream.send(message).map_err(|e| e.to_string())?;
            }
        } else {
            std::thread::sleep(SLICE);
        }
        if let Some(message) = read(&mut upstream)? {
            client.send(message).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn read<S: Read + Write>(socket: &mut WebSocket<S>) -> Result<Option<Message>, String> {
    match socket.read() {
        Ok(Message::Close(_)) => Err("closed".to_string()),
        Ok(message @ (Message::Text(_) | Message::Binary(_))) => Ok(Some(message)),
        Ok(_) => Ok(None),
        Err(tungstenite::Error::Io(e))
            if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
        {
            Ok(None)
        }
        Err(e) => Err(e.to_string()),
    }
}

//! Chrome process spawn + `/json/version` handshake (S4 Task 1).
//!
//! Owns the headless Chrome child, its debug port, and the profile dir.
//! `Drop` kills the child and removes the profile dir (best-effort).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Failures spawning or handshaking with headless Chrome.
#[derive(Debug)]
pub enum BrowserError {
    /// `Command::spawn` failed (message carries the OS error).
    Spawn(String),
    /// No free loopback port for `--remote-debugging-port`.
    NoPort,
    /// Chrome never answered `GET /json/version` (message carries detail).
    Handshake(String),
}

impl std::fmt::Display for BrowserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrowserError::Spawn(msg) => write!(f, "spawn chrome: {msg}"),
            BrowserError::NoPort => write!(f, "no free port for chrome"),
            BrowserError::Handshake(msg) => write!(f, "chrome handshake: {msg}"),
        }
    }
}

impl std::error::Error for BrowserError {}

/// Owned headless Chrome: child + debug port + browser-level CDP URL.
///
/// Killed and profile-removed on drop. [`Self::page_ws_url`] lists targets
/// over HTTP and returns the page-level WebSocket (sessions attach there —
/// browser-level flatten has no active page for screenshots).
pub struct BrowserProcess {
    child: std::process::Child,
    profile_dir: std::path::PathBuf,
    pub port: u16,
    pub browser_ws_url: String,
}

impl BrowserProcess {
    /// Spawn `exe` headless with its own `profile_dir`, wait (up to 60s)
    /// for `GET /json/version` to report `webSocketDebuggerUrl`.
    pub fn spawn(exe: &str, profile_dir: &Path) -> Result<BrowserProcess, BrowserError> {
        let port = free_port()?;
        let mut child = Command::new(exe)
            .arg("--headless=new")
            .arg(format!("--remote-debugging-port={port}"))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            // Skip component downloads on fresh profiles — cold start would
            // otherwise exceed the handshake deadline (observed >20s).
            .arg("--disable-component-update")
            .arg(format!("--user-data-dir={}", profile_dir.to_string_lossy()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| BrowserError::Spawn(e.to_string()))?;
        // Handshake failure must not orphan the child: a spawned-but-
        // unadopted Chrome holds the profile SingletonLock and breaks the
        // NEXT spawn on the same dir (fail-open exit → 60s of refused).
        let browser_ws_url = match poll_version(port) {
            Ok(url) => url,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };
        Ok(BrowserProcess {
            child,
            profile_dir: profile_dir.to_path_buf(),
            port,
            browser_ws_url,
        })
    }

    /// Page-level WebSocket URL: `GET /json/list`, first `type == "page"`.
    /// Page targets own an active page (screenshots work); the browser-level
    /// socket does not (flatten attach has no active page).
    pub fn page_ws_url(&self) -> Result<String, BrowserError> {
        let list = http_get(self.port, "/json/list")?;
        let v: serde_json::Value =
            serde_json::from_str(&list).map_err(|e| BrowserError::Handshake(e.to_string()))?;
        v.as_array()
            .and_then(|ts| {
                ts.iter()
                    .find(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
                    .or_else(|| ts.first())
            })
            .and_then(|t| t.get("webSocketDebuggerUrl"))
            .and_then(|u| u.as_str())
            .map(str::to_string)
            .ok_or_else(|| {
                BrowserError::Handshake("list: no page webSocketDebuggerUrl".to_string())
            })
    }
}

impl Drop for BrowserProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.profile_dir);
    }
}

/// Pick a free loopback port by binding `:0`, then release it.
fn free_port() -> Result<u16, BrowserError> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|_| BrowserError::NoPort)?;
    listener
        .local_addr()
        .map(|a| a.port())
        .map_err(|_| BrowserError::NoPort)
}

/// Poll `GET /json/version` until Chrome answers or 60s elapse (cold
/// first-run on a fresh profile takes ~30s even with component update off).
fn poll_version(port: u16) -> Result<String, BrowserError> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match fetch_ws_url(port) {
            Ok(url) => return Ok(url),
            Err(e) => {
                if Instant::now() >= deadline {
                    return Err(e);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// One `GET {path}` over a plain `TcpStream`, returning the body.
///
/// Chrome's DevTools server is picky (observed 154): HTTP/1.0 gets an empty
/// reply, and a missing/non-IP `Host` gets `500 Host header is ...`.
/// Read is bounded — the server may keep the connection open past the body,
/// so `read_to_end` would block until timeout.
fn http_get(port: u16, path: &str) -> Result<String, BrowserError> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .map_err(|e| BrowserError::Handshake(e.to_string()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| BrowserError::Handshake(e.to_string()))?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| BrowserError::Handshake(e.to_string()))?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            // Timeout/EAGAIN after headers+body arrived is normal (keep-alive
            // tail): fall through and parse what we have.
            Err(_) => break,
        }
        if raw.len() > 65536 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&raw);
    if !text.starts_with("HTTP/1.1 200 ") {
        return Err(BrowserError::Handshake(format!(
            "unexpected status: {:?}",
            &text[..text.len().min(80)]
        )));
    }
    text.split_once("\r\n\r\n")
        .map(|(_, body)| body.trim_end().to_string())
        .ok_or_else(|| BrowserError::Handshake("malformed http response".to_string()))
}

/// One `GET /json/version` attempt: parse `webSocketDebuggerUrl`.
fn fetch_ws_url(port: u16) -> Result<String, BrowserError> {
    let body = http_get(port, "/json/version")?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| BrowserError::Handshake(e.to_string()))?;
    v.get("webSocketDebuggerUrl")
        .and_then(|u| u.as_str())
        .map(str::to_string)
        .ok_or_else(|| BrowserError::Handshake("missing webSocketDebuggerUrl".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    const CHROME_EXE: &str = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
    #[cfg(not(target_os = "macos"))]
    const CHROME_EXE: &str = "google-chrome";

    #[test]
    #[ignore]
    fn spawn_answers_version() {
        // Unique profile per run: a stale SingletonLock from a previous
        // (possibly crashed) Chrome breaks reuse of a fixed dir.
        let dir = std::env::temp_dir().join(format!("cedian-browser-test-{}", std::process::id()));
        let proc = BrowserProcess::spawn(CHROME_EXE, &dir).unwrap();
        assert!(proc.browser_ws_url.starts_with("ws://127.0.0.1:"));
    }
}

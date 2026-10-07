//! `cedian_lsp`: headless LSP client over stdio (S1).
//!
//! Spawns a language server (`rust-analyzer` default), speaks LSP
//! (Content-Length framing, bulk `os.read` — never byte-loop: macOS pipes +
//! per-byte reads stall for minutes), collects `publishDiagnostics`, answers
//! hover/definition/documentSymbol/workspaceSymbol.
//!
//! Blocking API for a dedicated thread. Floor: initialize → initialized →
//! didOpen; diagnostics arrive as server notifications.
#![allow(
    clippy::disallowed_methods,
    reason = "headless, synchronous process control (OMP, LSP, DAP, Chrome, git, sandbox-exec): Zed's async spawn helpers do not apply"
)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Read;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::Duration;

/// LSP position (0-based, UTF-16 — matches rust-analyzer default encoding).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// LSP range.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// One diagnostic (subset — what cedian surfaces).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspDiagnostic {
    pub range: Range,
    pub severity: Option<u32>,
    pub code: Option<Value>,
    pub source: Option<String>,
    pub message: String,
}

/// Symbol (document + workspace).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspSymbol {
    pub name: String,
    pub kind: u32,
    pub location: Value,
    #[serde(default)]
    pub container: Option<String>,
}

/// Hover result as markdown/plain text.
#[derive(Debug, Clone)]
pub struct Hover {
    pub text: String,
}

/// Client failures (explicit, caller-visible).
#[derive(Debug)]
pub enum LspError {
    Spawn(String),
    Io(String),
    Protocol(String),
    Timeout { method: String },
    Server { code: i64, message: String },
    Shutdown,
}

impl std::fmt::Display for LspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "lsp spawn: {e}"),
            Self::Io(e) => write!(f, "lsp io: {e}"),
            Self::Protocol(e) => write!(f, "lsp protocol: {e}"),
            Self::Timeout { method } => write!(f, "lsp {method} timed out"),
            Self::Server { code, message } => write!(f, "lsp server error {code}: {message}"),
            Self::Shutdown => write!(f, "lsp client shut down"),
        }
    }
}

impl std::error::Error for LspError {}

/// Unsolicited server → client frames the owner cares about.
#[derive(Debug, Clone)]
pub enum LspNotification {
    Diagnostics {
        uri: String,
        diagnostics: Vec<LspDiagnostic>,
    },
    Other {
        method: String,
    },
}

type RespTx = mpsc::Sender<Result<Value, LspError>>;

struct Pending {
    next_id: AtomicU64,
    waiters: parking_lot::Mutex<HashMap<u64, RespTx>>,
}

/// Headless LSP client: owns the server child + reader/writer threads.
/// Notifications broadcast to subscribers (each gets its own channel); the
/// broadcast list is shared with the reader thread, so `&LspClient` is `Sync`.
pub struct LspClient {
    child: Option<Child>,
    stdin_tx: mpsc::Sender<Vec<u8>>,
    pending: Arc<Pending>,
    broadcast: Arc<parking_lot::Mutex<Vec<mpsc::Sender<LspNotification>>>>,
    _reader: JoinHandle<()>,
    _writer: JoinHandle<()>,
    timeout: Duration,
}

// SAFETY: shared state is behind `Arc<Mutex|Pending>` + channels; `Child` and
// thread handles are only touched via message passing.
#[allow(unsafe_code)]
unsafe impl Sync for LspClient {}

impl LspClient {
    /// Spawn `server_cmd` with `workdir` as cwd, initialize for `root_uri`.
    /// Waits for the initialize response (server warmup included).
    pub fn spawn(server_cmd: &str, workdir: &str, root_uri: &str) -> Result<Self, LspError> {
        Self::spawn_with_timeout(
            server_cmd,
            workdir,
            root_uri,
            // Cold rust-analyzer init (cargo metadata + first check) can take
            // minutes on first launch of the day; warm restarts answer in <1s.
            Duration::from_secs(900),
        )
    }

    /// Same with an explicit initialize deadline.
    pub fn spawn_with_timeout(
        server_cmd: &str,
        workdir: &str,
        root_uri: &str,
        timeout: Duration,
    ) -> Result<Self, LspError> {
        let mut child = Command::new(server_cmd)
            .arg("--log-file")
            .arg("/dev/null")
            .current_dir(workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| LspError::Spawn(e.to_string()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| LspError::Spawn("no stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LspError::Spawn("no stdout".to_string()))?;
        Self::connect(stdout, stdin, Some(child), timeout, root_uri)
    }
    fn connect(
        stdout: impl Read + Send + 'static,
        stdin: ChildStdin,
        child: Option<Child>,
        timeout: Duration,
        root_uri: &str,
    ) -> Result<Self, LspError> {
        let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>();
        let _writer = std::thread::spawn(move || {
            let mut stdin: ChildStdin = stdin;
            use std::io::Write;
            for frame in stdin_rx {
                if stdin.write_all(&frame).is_err() {
                    break;
                }
                if stdin.flush().is_err() {
                    break;
                }
            }
        });

        let pending = Arc::new(Pending {
            next_id: AtomicU64::new(1),
            waiters: parking_lot::Mutex::new(HashMap::new()),
        });
        let broadcast: Arc<parking_lot::Mutex<Vec<mpsc::Sender<LspNotification>>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let reader_pending = Arc::clone(&pending);
        let reader_broadcast = Arc::clone(&broadcast);
        let _reader = std::thread::spawn(move || {
            reader_loop(stdout, &reader_pending, &reader_broadcast);
        });

        let client = Self {
            child,
            stdin_tx,
            pending,
            broadcast,
            _reader,
            _writer,
            timeout,
        };
        let params = serde_json::json!({
            "processId": null,
            "rootUri": root_uri,
            "capabilities": {},
        });
        client.request("initialize", params)?;
        let _ = client.notify("initialized", serde_json::json!({}));
        Ok(client)
    }
    /// Spawn an argv (generic servers: fakes, other languages — no
    /// rust-analyzer `--log-file` flag).
    pub fn spawn_argv(argv: &[&str], workdir: &str, root_uri: &str) -> Result<Self, LspError> {
        Self::spawn_argv_with_timeout(argv, workdir, root_uri, Duration::from_secs(120))
    }

    /// Same with an explicit initialize deadline.
    pub fn spawn_argv_with_timeout(
        argv: &[&str],
        workdir: &str,
        root_uri: &str,
        timeout: Duration,
    ) -> Result<Self, LspError> {
        let (prog, args) = argv
            .split_first()
            .ok_or_else(|| LspError::Spawn("empty argv".to_string()))?;
        let mut child = Command::new(prog)
            .args(args)
            .current_dir(workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| LspError::Spawn(e.to_string()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| LspError::Spawn("no stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LspError::Spawn("no stdout".to_string()))?;
        Self::connect(stdout, stdin, Some(child), timeout, root_uri)
    }

    /// Open (or update) a document's text.
    pub fn did_open(&self, uri: &str, language_id: &str, version: i32, text: &str) {
        let _ = self.notify(
            "textDocument/didOpen",
            serde_json::json!({"textDocument": {"uri": uri, "languageId": language_id, "version": version, "text": text}}),
        );
    }

    /// Save notification (triggers check-on-save diagnostics on servers that
    /// only check on save; harmless otherwise).
    pub fn did_save(&self, uri: &str, text: Option<&str>) {
        let mut doc = serde_json::json!({"uri": uri});
        if let Some(text) = text {
            doc["text"] = Value::String(text.to_string());
        }
        let _ = self.notify(
            "textDocument/didSave",
            serde_json::json!({"textDocument": doc}),
        );
    }

    /// Subscribe to server notifications (diagnostics, etc.). Each subscriber
    /// gets every notification; dead receivers are pruned on broadcast.
    pub fn subscribe(&self) -> mpsc::Receiver<LspNotification> {
        let (tx, rx) = mpsc::channel();
        self.broadcast.lock().push(tx);
        rx
    }

    /// Next server notification (diagnostics, etc.), or `None` on timeout.
    /// Convenience over a private subscription; prefer `subscribe` for pumps.
    pub fn next_notification(&self, timeout: Duration) -> Option<LspNotification> {
        let rx = self.subscribe();
        rx.recv_timeout(timeout).ok()
    }

    /// Hover at a position; `None` when the server has nothing.
    pub fn hover(&self, uri: &str, pos: Position) -> Result<Option<Hover>, LspError> {
        let result = self.request(
            "textDocument/hover",
            serde_json::json!({"textDocument": {"uri": uri}, "position": pos}),
        )?;
        if result.is_null() {
            return Ok(None);
        }
        let text = extract_marked_string(&result);
        Ok(Some(Hover { text }))
    }

    /// Go-to-definition locations (raw JSON — caller renders).
    pub fn definition(&self, uri: &str, pos: Position) -> Result<Vec<Value>, LspError> {
        let result = self.request(
            "textDocument/definition",
            serde_json::json!({"textDocument": {"uri": uri}, "position": pos}),
        )?;
        Ok(as_array(result))
    }

    /// Document symbols for an open file.
    pub fn document_symbols(&self, uri: &str) -> Result<Vec<LspSymbol>, LspError> {
        let result = self.request(
            "textDocument/documentSymbol",
            serde_json::json!({"textDocument": {"uri": uri}}),
        )?;
        Ok(as_array(result)
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect())
    }

    /// Workspace-wide symbol search.
    pub fn workspace_symbols(&self, query: &str) -> Result<Vec<LspSymbol>, LspError> {
        let result = self.request("workspace/symbol", serde_json::json!({"query": query}))?;
        Ok(as_array(result)
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect())
    }

    /// Shut down: exit notification + stdin EOF (server exits), then reap.
    pub fn shutdown(mut self) -> Result<(), LspError> {
        let _ = self.notify("exit", Value::Null);
        drop(self.stdin_tx);
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
        Ok(())
    }

    fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        let id = self.pending.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.pending.waiters.lock().insert(id, tx);
        let body =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send_frame(&body)?;
        rx.recv_timeout(self.timeout).map_err(|_| {
            self.pending.waiters.lock().remove(&id);
            LspError::Timeout {
                method: method.to_string(),
            }
        })?
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        let body = serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.send_frame(&body)
    }

    fn send_frame(&self, body: &Value) -> Result<(), LspError> {
        let raw = serde_json::to_vec(body).map_err(|e| LspError::Protocol(e.to_string()))?;
        if std::env::var("CEDIAN_LSP_WIRE_LOG").is_ok() {
            eprintln!(
                "[wire] -> {}",
                &String::from_utf8_lossy(&raw)[..300.min(raw.len())]
            );
        }
        let mut frame = format!("Content-Length: {}\r\n\r\n", raw.len()).into_bytes();
        frame.extend_from_slice(&raw);
        self.stdin_tx.send(frame).map_err(|_| LspError::Shutdown)
    }
}

fn as_array(v: Value) -> Vec<Value> {
    match v {
        Value::Array(items) => items,
        Value::Null => Vec::new(),
        single => vec![single],
    }
}

fn extract_marked_string(v: &Value) -> String {
    if let Some(s) = v.get("contents").and_then(|c| c.as_str()) {
        return s.to_string();
    }
    if let Some(kind) = v
        .get("contents")
        .and_then(|c| c.get("kind"))
        .and_then(|k| k.as_str())
    {
        let val = v
            .get("contents")
            .and_then(|c| c.get("value"))
            .and_then(|x| x.as_str())
            .unwrap_or("");
        if kind == "markdown" {
            return val.to_string();
        }
    }
    v.to_string()
}

/// Reader: bulk `Read::read` framing in 64KiB chunks (NEVER byte-loop — macOS
/// pipe stall), responses → waiters by id, notifications → broadcast list
/// (dead receivers pruned).
fn reader_loop(
    mut stdout: impl Read + Send + 'static,
    pending: &Arc<Pending>,
    broadcast: &Arc<parking_lot::Mutex<Vec<mpsc::Sender<LspNotification>>>>,
) {
    let mut buf = Vec::new();
    loop {
        let mut chunk = [0u8; 65536];
        let n = match stdout.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        while let Some(frame) = take_frame(&mut buf) {
            if std::env::var("CEDIAN_LSP_WIRE_LOG").is_ok() {
                eprintln!(
                    "[wire] <- {}",
                    frame
                        .get("method")
                        .and_then(|m| m.as_str())
                        .unwrap_or(frame.get("command").and_then(|c| c.as_str()).unwrap_or("?"))
                );
            }
            dispatch_frame(pending, broadcast, frame);
        }
    }
}

fn take_frame(buf: &mut Vec<u8>) -> Option<Value> {
    let header_end = find_header_end(buf)?;
    let header = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let len: usize = header.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        if k.trim().eq_ignore_ascii_case("content-length") {
            v.trim().parse().ok()
        } else {
            None
        }
    })?;
    let body_start = header_end;
    if buf.len() < body_start + len {
        return None;
    }
    let body = buf[body_start..body_start + len].to_vec();
    buf.drain(..body_start + len);
    match serde_json::from_slice::<Value>(&body) {
        Ok(v) => Some(v),
        Err(e) => {
            if std::env::var("CEDIAN_LSP_WIRE_LOG").is_ok() {
                eprintln!(
                    "[wire] PARSE-FAIL {}: {}",
                    e,
                    String::from_utf8_lossy(&body[..200.min(body.len())])
                );
            }
            // Return a sentinel so the reader loop keeps draining (don't stall
            // on one bad frame, don't silently swallow either).
            Some(Value::Null)
        }
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

fn dispatch_frame(
    pending: &Arc<Pending>,
    broadcast: &Arc<parking_lot::Mutex<Vec<mpsc::Sender<LspNotification>>>>,
    frame: Value,
) {
    if let Some(id) = frame.get("id").and_then(|i| i.as_u64()) {
        let tx = pending.waiters.lock().remove(&id);
        if let Some(tx) = tx {
            if frame.get("error").is_some() {
                let code = frame["error"]
                    .get("code")
                    .and_then(|c| c.as_i64())
                    .unwrap_or(-1);
                let message = frame["error"]
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("?")
                    .to_string();
                let _ = tx.send(Err(LspError::Server { code, message }));
            } else {
                let result = frame.get("result").cloned().unwrap_or(Value::Null);
                let _ = tx.send(Ok(result));
            }
        }
        return;
    }
    if let Some(method) = frame.get("method").and_then(|m| m.as_str()) {
        let notif = if method == "textDocument/publishDiagnostics" {
            let uri = frame["params"]
                .get("uri")
                .and_then(|u| u.as_str())
                .unwrap_or("")
                .to_string();
            let diagnostics: Vec<LspDiagnostic> = match frame["params"].get("diagnostics") {
                Some(d) => match serde_json::from_value(d.clone()) {
                    Ok(v) => v,
                    Err(e) => {
                        if std::env::var("CEDIAN_LSP_WIRE_LOG").is_ok() {
                            eprintln!("[wire] DIAG-PARSE-FAIL: {e}");
                        }
                        Vec::new()
                    }
                },
                None => Vec::new(),
            };
            LspNotification::Diagnostics { uri, diagnostics }
        } else {
            LspNotification::Other {
                method: method.to_string(),
            }
        };
        broadcast.lock().retain(|tx| tx.send(notif.clone()).is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_rust_analyzer_diagnostic_parses() {
        // Exact shape from rust-analyzer 1.99 publishDiagnostics (E0308).
        let raw = serde_json::json!([{
            "range": {"start": {"line": 1, "character": 17}, "end": {"line": 1, "character": 23}},
            "severity": 1,
            "code": "E0308",
            "codeDescription": {"href": "https://doc.rust-lang.org/error-index.html#E0308"},
            "source": "rustc",
            "message": "mismatched types\nexpected `i32`, found `&str`",
            "relatedInformation": [],
            "data": {"rendered": "error[E0308]: ..."}
        }]);
        let diags: Vec<LspDiagnostic> = serde_json::from_value(raw).expect("parses");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].range.start.line, 1);
    }
}

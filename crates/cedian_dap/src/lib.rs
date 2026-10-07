//! `cedian_dap`: headless Debug Adapter Protocol client (S1).
//!
//! Spawns `lldb-dap` over stdio with DAP framing (Content-Length, bulk reads —
//! same discipline as `cedian_lsp`). Covers the S1 exit: launch a program,
//! set a breakpoint, read stack + variables on stop. Stepping/continue
//! included; conditional breakpoints + watchpoints are follow-ups.
//!
//! Blocking API for a dedicated thread. Events (stopped, terminated, output)
//! arrive on the notification channel.
#![allow(
    clippy::disallowed_methods,
    reason = "headless, synchronous process control (OMP, LSP, DAP, Chrome, git, sandbox-exec): Zed's async spawn helpers do not apply"
)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::Duration;

/// DAP breakpoint location (verified flag comes back from the server).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Breakpoint {
    pub id: Option<u64>,
    pub verified: bool,
    pub line: Option<u32>,
    pub message: Option<String>,
}

/// One stack frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackFrame {
    pub id: u64,
    pub name: String,
    pub line: u32,
    pub column: u32,
    #[serde(default)]
    pub source: Option<Value>,
}

/// One scope in a stack frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scope {
    #[serde(rename = "variablesReference")]
    pub variables_reference: u64,
    pub name: String,
}

/// One variable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variable {
    pub name: String,
    pub value: String,
    #[serde(rename = "variablesReference")]
    pub variables_reference: u64,
}

/// Server → client events the owner handles.
#[derive(Debug, Clone)]
pub enum DapEvent {
    Stopped {
        reason: String,
        thread_id: Option<u64>,
    },
    Continued {
        thread_id: u64,
    },
    Terminated,
    Exited {
        code: u32,
    },
    Output {
        text: String,
    },
    Other {
        event: String,
    },
}

/// Client failures (explicit, caller-visible).
#[derive(Debug)]
pub enum DapError {
    Spawn(String),
    Io(String),
    Protocol(String),
    Timeout { command: String },
    Server(String),
    Shutdown,
}

impl std::fmt::Display for DapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "dap spawn: {e}"),
            Self::Io(e) => write!(f, "dap io: {e}"),
            Self::Protocol(e) => write!(f, "dap protocol: {e}"),
            Self::Timeout { command } => write!(f, "dap {command} timed out"),
            Self::Server(e) => write!(f, "dap server: {e}"),
            Self::Shutdown => write!(f, "dap client shut down"),
        }
    }
}

impl std::error::Error for DapError {}

type RespTx = mpsc::Sender<Result<Value, DapError>>;

struct Pending {
    next_seq: AtomicU64,
    waiters: parking_lot::Mutex<HashMap<u64, RespTx>>,
}

/// Headless DAP client: owns the adapter child + reader/writer threads.
pub struct DapClient {
    child: Option<Child>,
    stdin_tx: mpsc::Sender<Vec<u8>>,
    pending: Arc<Pending>,
    event_rx: mpsc::Receiver<DapEvent>,
    _reader: JoinHandle<()>,
    _writer: JoinHandle<()>,
    timeout: Duration,
}

impl DapClient {
    /// Spawn `adapter_cmd` and initialize. Waits for the initialize response.
    pub fn spawn(adapter_cmd: &str) -> Result<Self, DapError> {
        Self::spawn_with_timeout(adapter_cmd, Duration::from_secs(60))
    }

    /// Same with an explicit initialize deadline.
    pub fn spawn_with_timeout(adapter_cmd: &str, timeout: Duration) -> Result<Self, DapError> {
        let mut child = Command::new(adapter_cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| DapError::Spawn(e.to_string()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| DapError::Spawn("no stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| DapError::Spawn("no stdout".to_string()))?;
        Self::connect(stdout, stdin, Some(child), timeout)
    }

    /// Connect over an existing reader/writer pair (hermetic tests, custom
    /// transports). No child to reap — `shutdown` just closes the channels.
    /// Sends `initialize` and waits for the response like `spawn` does.
    pub fn from_io<R, W>(reader: R, writer: W) -> Result<Self, DapError>
    where
        R: Read + Send + 'static,
        W: std::io::Write + Send + 'static,
    {
        Self::connect(reader, writer, None, Duration::from_secs(30))
    }

    fn connect<R, W>(
        reader: R,
        writer: W,
        child: Option<Child>,
        timeout: Duration,
    ) -> Result<Self, DapError>
    where
        R: Read + Send + 'static,
        W: std::io::Write + Send + 'static,
    {
        let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>();
        let _writer = std::thread::spawn(move || {
            let mut writer = writer;
            for frame in stdin_rx {
                if writer.write_all(&frame).is_err() {
                    break;
                }
                if writer.flush().is_err() {
                    break;
                }
            }
        });

        let pending = Arc::new(Pending {
            next_seq: AtomicU64::new(1),
            waiters: parking_lot::Mutex::new(HashMap::new()),
        });
        let (event_tx, event_rx) = mpsc::channel();
        let reader_pending = Arc::clone(&pending);
        let _reader = std::thread::spawn(move || {
            reader_loop(reader, &reader_pending, &event_tx);
        });

        let client = Self {
            child,
            stdin_tx,
            pending,
            event_rx,
            _reader,
            _writer,
            timeout,
        };
        client.request(
            "initialize",
            serde_json::json!({"adapterID": "cedian", "pathFormat": "path"}),
        )?;
        Ok(client)
    }

    /// Next adapter event, or `None` on timeout.
    pub fn next_event(&self, timeout: Duration) -> Option<DapEvent> {
        self.event_rx.recv_timeout(timeout).ok()
    }

    /// Launch a program (waits for the launch response, NOT for stop).
    pub fn launch(&self, program: &str, args: &[&str], cwd: &str) -> Result<(), DapError> {
        self.request(
            "launch",
            serde_json::json!({"program": program, "args": args, "cwd": cwd, "stopOnEntry": false}),
        )?;
        Ok(())
    }

    /// Attach-to-process variant is out of scope for S1 (launch only).
    /// Set breakpoints for one source file. Returns the server-verified list.
    pub fn set_breakpoints(&self, path: &str, lines: &[u32]) -> Result<Vec<Breakpoint>, DapError> {
        let bps: Vec<Value> = lines
            .iter()
            .map(|l| serde_json::json!({"line": l}))
            .collect();
        let result = self.request(
            "setBreakpoints",
            serde_json::json!({"source": {"path": path}, "breakpoints": bps}),
        )?;
        let list = result
            .get("breakpoints")
            .cloned()
            .unwrap_or(Value::Array(vec![]));
        Ok(serde_json::from_value::<Vec<Breakpoint>>(list).unwrap_or_default())
    }

    /// ConfigurationDone (required after breakpoints, before launch resumes).
    pub fn configuration_done(&self) -> Result<(), DapError> {
        self.request("configurationDone", serde_json::json!({}))?;
        Ok(())
    }

    /// Threads list (raw — caller picks the stopped thread).
    pub fn threads(&self) -> Result<Value, DapError> {
        self.request("threads", serde_json::json!({}))
    }

    /// Stack trace for a thread.
    pub fn stack_trace(&self, thread_id: u64) -> Result<Vec<StackFrame>, DapError> {
        let result = self.request("stackTrace", serde_json::json!({"threadId": thread_id}))?;
        let frames = result
            .get("stackFrames")
            .cloned()
            .unwrap_or(Value::Array(vec![]));
        Ok(serde_json::from_value::<Vec<StackFrame>>(frames).unwrap_or_default())
    }

    /// Scopes for a frame.
    pub fn scopes(&self, frame_id: u64) -> Result<Vec<Scope>, DapError> {
        let result = self.request("scopes", serde_json::json!({"frameId": frame_id}))?;
        let list = result
            .get("scopes")
            .cloned()
            .unwrap_or(Value::Array(vec![]));
        Ok(serde_json::from_value::<Vec<Scope>>(list).unwrap_or_default())
    }

    /// Variables for a reference.
    pub fn variables(&self, reference: u64) -> Result<Vec<Variable>, DapError> {
        let result = self.request(
            "variables",
            serde_json::json!({"variablesReference": reference}),
        )?;
        let list = result
            .get("variables")
            .cloned()
            .unwrap_or(Value::Array(vec![]));
        Ok(serde_json::from_value::<Vec<Variable>>(list).unwrap_or_default())
    }

    /// Continue (all threads by default in lldb-dap).
    pub fn continue_(&self, thread_id: u64) -> Result<(), DapError> {
        self.request("continue", serde_json::json!({"threadId": thread_id}))?;
        Ok(())
    }

    /// Next step (over).
    pub fn next(&self, thread_id: u64) -> Result<(), DapError> {
        self.request("next", serde_json::json!({"threadId": thread_id}))?;
        Ok(())
    }

    /// Disconnect (terminates the debuggee) + reap the adapter.
    pub fn shutdown(mut self) -> Result<(), DapError> {
        let _ = self.raw("disconnect", serde_json::json!({"terminateDebuggee": true}));
        drop(self.stdin_tx);
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
        Ok(())
    }

    fn request(&self, command: &str, arguments: Value) -> Result<Value, DapError> {
        let seq = self.pending.next_seq.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.pending.waiters.lock().insert(seq, tx);
        let body = serde_json::json!({"seq": seq, "type": "request", "command": command, "arguments": arguments});
        self.send_frame(&body)?;
        rx.recv_timeout(self.timeout).map_err(|_| {
            self.pending.waiters.lock().remove(&seq);
            DapError::Timeout {
                command: command.to_string(),
            }
        })?
    }

    fn raw(&self, command: &str, arguments: Value) -> Result<Value, DapError> {
        self.request(command, arguments)
    }

    fn send_frame(&self, body: &Value) -> Result<(), DapError> {
        let raw = serde_json::to_vec(body).map_err(|e| DapError::Protocol(e.to_string()))?;
        let mut frame = format!("Content-Length: {}\r\n\r\n", raw.len()).into_bytes();
        frame.extend_from_slice(&raw);
        self.stdin_tx.send(frame).map_err(|_| DapError::Shutdown)
    }
}

/// Reader: bulk reads in 64KiB chunks (same macOS-pipe discipline as LSP),
/// responses → waiters by `request_seq`, events → channel.
fn reader_loop(
    mut stdout: impl Read + Send + 'static,
    pending: &Arc<Pending>,
    event_tx: &mpsc::Sender<DapEvent>,
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
            dispatch_frame(pending, event_tx, frame);
        }
    }
}

fn take_frame(buf: &mut Vec<u8>) -> Option<Value> {
    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)?;
    let header = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let len: usize = header.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        if k.trim().eq_ignore_ascii_case("content-length") {
            v.trim().parse().ok()
        } else {
            None
        }
    })?;
    if buf.len() < header_end + len {
        return None;
    }
    let body = buf[header_end..header_end + len].to_vec();
    buf.drain(..header_end + len);
    serde_json::from_slice(&body).ok()
}

fn dispatch_frame(pending: &Arc<Pending>, event_tx: &mpsc::Sender<DapEvent>, frame: Value) {
    let ftype = frame.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if ftype == "response" {
        let seq = frame
            .get("request_seq")
            .and_then(|s| s.as_u64())
            .unwrap_or(0);
        let tx = pending.waiters.lock().remove(&seq);
        if let Some(tx) = tx {
            if frame.get("success").and_then(|s| s.as_bool()) == Some(false) {
                let msg = frame
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("?")
                    .to_string();
                let _ = tx.send(Err(DapError::Server(msg)));
            } else {
                let body = frame.get("body").cloned().unwrap_or(Value::Null);
                let _ = tx.send(Ok(body));
            }
        }
        return;
    }
    if ftype == "event" {
        let event = frame.get("event").and_then(|e| e.as_str()).unwrap_or("");
        let body = frame.get("body").cloned().unwrap_or(Value::Null);
        let ev = match event {
            "stopped" => DapEvent::Stopped {
                reason: body
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("?")
                    .to_string(),
                thread_id: body.get("threadId").and_then(|t| t.as_u64()),
            },
            "continued" => DapEvent::Continued {
                thread_id: body.get("threadId").and_then(|t| t.as_u64()).unwrap_or(0),
            },
            "terminated" => DapEvent::Terminated,
            "exited" => DapEvent::Exited {
                code: body.get("exitCode").and_then(|c| c.as_u64()).unwrap_or(0) as u32,
            },
            "output" => DapEvent::Output {
                text: body
                    .get("output")
                    .and_then(|o| o.as_str())
                    .unwrap_or("")
                    .to_string(),
            },
            other => DapEvent::Other {
                event: other.to_string(),
            },
        };
        let _ = event_tx.send(ev);
    }
}

/// Hermetic fake adapter for unit tests (§86: no real adapter, no model, ever).
/// Speaks DAP framing over an in-memory pipe pair: answers initialize →
/// launch → setBreakpoints → configurationDone → stackTrace/scopes/variables →
/// continue with canned data, emitting `stopped` (breakpoint) then `exited`
/// (code 15). The client connects via [`DapClient::from_io`].
/// Hermetic fake adapter (feature `test-utils`, compiled for tests).
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils {
    use super::*;
    use std::io::{Read, Write};

    /// Client-side transport ends. Move both into `DapClient::from_io`.
    pub struct FakeTransport {
        pub reader: FakeReader,
        pub writer: FakeWriter,
    }

    /// Test-only byte pipe ends (in-memory, no fd).
    pub struct FakeReader {
        rx: mpsc::Receiver<Vec<u8>>,
        buf: Vec<u8>,
    }

    pub struct FakeWriter {
        tx: mpsc::Sender<Vec<u8>>,
    }

    impl Read for FakeReader {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            while self.buf.is_empty() {
                match self.rx.recv() {
                    Ok(chunk) => self.buf.extend_from_slice(&chunk),
                    Err(_) => return Ok(0),
                }
            }
            let n = out.len().min(self.buf.len());
            out[..n].copy_from_slice(&self.buf[..n]);
            self.buf.drain(..n);
            Ok(n)
        }
    }

    impl Write for FakeWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.tx
                .send(buf.to_vec())
                .map(|_| buf.len())
                .map_err(|_| std::io::Error::other("fake closed"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl FakeTransport {
        /// Spawn the fake adapter thread and return the client ends.
        pub fn spawn() -> Self {
            let (c2s_tx, c2s_rx) = mpsc::channel::<Vec<u8>>();
            let (s2c_tx, s2c_rx) = mpsc::channel::<Vec<u8>>();
            std::thread::spawn(move || fake_adapter_main(c2s_rx, s2c_tx));
            Self {
                reader: FakeReader {
                    rx: s2c_rx,
                    buf: Vec::new(),
                },
                writer: FakeWriter { tx: c2s_tx },
            }
        }
    }

    fn send(tx: &mpsc::Sender<Vec<u8>>, obj: &Value) {
        let raw = serde_json::to_vec(obj).unwrap();
        let mut frame = format!("Content-Length: {}\r\n\r\n", raw.len()).into_bytes();
        frame.extend_from_slice(&raw);
        let _ = tx.send(frame);
    }

    fn read_frame(rx: &mpsc::Receiver<Vec<u8>>, buf: &mut Vec<u8>) -> Option<Value> {
        loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let header = String::from_utf8_lossy(&buf[..i]).into_owned();
                let len: usize = header.lines().find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    if k.trim().eq_ignore_ascii_case("content-length") {
                        v.trim().parse().ok()
                    } else {
                        None
                    }
                })?;
                if buf.len() >= i + 4 + len {
                    let body = buf[i + 4..i + 4 + len].to_vec();
                    *buf = buf[i + 4 + len..].to_vec();
                    return serde_json::from_slice(&body).ok();
                }
            }
            match rx.recv() {
                Ok(chunk) => buf.extend_from_slice(&chunk),
                Err(_) => return None,
            }
        }
    }

    fn fake_adapter_main(rx: mpsc::Receiver<Vec<u8>>, tx: mpsc::Sender<Vec<u8>>) {
        let mut buf = Vec::new();
        let seq = std::cell::Cell::new(100u64);
        let event = |ev: &str, body: Value| {
            let s = seq.get() + 1;
            seq.set(s);
            send(
                &tx,
                &serde_json::json!({"seq": s, "type": "event", "event": ev, "body": body}),
            );
        };
        while let Some(req) = read_frame(&rx, &mut buf) {
            let cmd = req.get("command").and_then(|c| c.as_str()).unwrap_or("");
            let req_seq = req.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);
            let s = seq.get() + 1;
            seq.set(s);
            let body: Value = match cmd {
                "initialize" => serde_json::json!({}),
                "launch" => serde_json::json!({}),
                "setBreakpoints" => {
                    serde_json::json!({"breakpoints": [{"id": 1, "verified": true, "line": 10}]})
                }
                "configurationDone" => {
                    event(
                        "stopped",
                        serde_json::json!({"reason": "breakpoint", "threadId": 7}),
                    );
                    serde_json::json!({})
                }
                "threads" => serde_json::json!({"threads": [{"id": 7, "name": "main"}]}),
                "stackTrace" => {
                    serde_json::json!({"stackFrames": [{"id": 11, "name": "main", "line": 10, "column": 5}], "totalFrames": 1})
                }
                "scopes" => {
                    serde_json::json!({"scopes": [{"variablesReference": 21, "name": "Locals"}]})
                }
                "variables" => {
                    serde_json::json!({"variables": [{"name": "total", "value": "15", "variablesReference": 0}]})
                }
                "continue" => {
                    event("exited", serde_json::json!({"exitCode": 15}));
                    serde_json::json!({"allThreadsContinued": true})
                }
                "disconnect" => serde_json::json!({}),
                _ => serde_json::json!({}),
            };
            send(
                &tx,
                &serde_json::json!({"seq": s, "type": "response", "request_seq": req_seq, "command": cmd, "success": true, "body": body}),
            );
            if cmd == "disconnect" {
                break;
            }
        }
    }
}

//! One Chromium per workspace, owned by the app and shared with OMP
//! (ADR-0049, §25, §29 R4).
//!
//! cedian listens on a loopback endpoint and hands it to OMP as
//! `browser.cdpUrl`. The first connection there (OMP's, or the panel's
//! "Open browser") starts Chromium; every connection after that is
//! forwarded to it. cedian keeps its own CDP line to the page for frame
//! sequence numbers, captures, console and network, and to see the person's
//! own input: while a turn runs, that input holds every message OMP sends to
//! the browser until the person lets the agent continue.

mod cdp;
mod chromium;
mod endpoint;
#[cfg(any(test, feature = "test-support"))]
pub mod fake;
mod monitor;

use cedian_workflow::{Evidence, EvidenceKind, Outcome};
use chromium::Chromium;
use parking_lot::{Condvar, Mutex};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

pub use chromium::executable;

/// Slack around an agent `Input.*` command's send and reply when matching a
/// trusted input event's time to it.
const INPUT_SLACK_MS: f64 = 10.0;
/// How long an answered agent input is kept for matching late reports.
const INPUT_KEEP_MS: f64 = 5000.0;

/// One `Input.*` command OMP sent through the endpoint: its connection and
/// id, sent and answered in wall-clock milliseconds.
#[derive(Debug, Clone)]
struct AgentInput {
    connection: u64,
    id: u64,
    sent: f64,
    answered: Option<f64>,
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or_default()
}

/// One capture of the shared page, bound to the browser's frame sequence.
#[derive(Debug, Clone)]
pub struct Capture {
    pub frame_id: String,
    /// The browser-wide sequence at capture; a later main-frame navigation
    /// moves the browser past it, and evidence from it reads `stale-frame`.
    pub seq: u64,
    pub png: Arc<Vec<u8>>,
    pub url: String,
    pub title: String,
    pub dom: String,
    pub console: Vec<String>,
    pub network: Vec<String>,
}

impl Capture {
    /// Evidence for `gates` from this capture, bound to its frame.
    pub fn evidence(&self, id: &str, gates: &[&str], outcome: Outcome) -> Evidence {
        let mut evidence = Evidence::unattributed(
            id,
            EvidenceKind::Screenshot,
            gates,
            format!("{} at frame {}", self.url, self.seq),
            outcome,
        );
        evidence.frame_seq = Some(self.seq);
        evidence
    }
}

/// What the panel shows.
#[derive(Debug, Clone, Default)]
pub struct BrowserState {
    pub running: bool,
    pub error: Option<String>,
    pub frame_id: String,
    pub seq: u64,
    pub url: String,
    /// Console and network lines since the last main-frame navigation.
    pub console: Vec<String>,
    pub network: Vec<String>,
    pub latest: Option<Capture>,
    pub turn_active: bool,
    /// The person used the window during this turn: OMP's browser traffic
    /// is held until [`BrowserHost::resume`] or the turn ends.
    pub preempted: bool,
    agent_inputs: Vec<AgentInput>,
}

struct Running {
    chromium: Chromium,
    captures: mpsc::Sender<mpsc::Sender<Result<Capture, String>>>,
}

struct Shared {
    exe: Option<PathBuf>,
    profile: PathBuf,
    running: Mutex<Option<Running>>,
    /// A launch is under way; [`Self::launched`] wakes those waiting on it.
    starting: Mutex<bool>,
    launched: Condvar,
    state: Mutex<BrowserState>,
    closed: AtomicBool,
    changed: Box<dyn Fn() + Send + Sync>,
}

/// The workspace's browser. Dropping it closes the endpoint and Chromium.
pub struct BrowserHost {
    shared: Arc<Shared>,
    port: u16,
}

impl BrowserHost {
    /// Listen on a loopback port; Chromium starts on the first connection.
    /// `changed` runs on any thread whenever [`Self::state`] changes.
    pub fn open(
        profile: PathBuf,
        exe: Option<PathBuf>,
        changed: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let listener =
            TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("browser endpoint: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let shared = Arc::new(Shared {
            exe,
            profile,
            running: Mutex::new(None),
            starting: Mutex::new(false),
            launched: Condvar::new(),
            state: Mutex::new(BrowserState::default()),
            closed: AtomicBool::new(false),
            changed: Box::new(changed),
        });
        endpoint::serve(listener, shared.clone());
        Ok(Self { shared, port })
    }

    /// The HTTP CDP endpoint OMP is given as `browser.cdpUrl`.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn state(&self) -> BrowserState {
        self.shared.state.lock().clone()
    }

    /// Start Chromium now if it is not running. Blocks while it starts.
    pub fn start(&self) -> Result<(), String> {
        self.shared.start().map(|_| ())
    }

    /// Capture the page: screenshot, DOM, console and network.
    pub fn capture(&self) -> Result<Capture, String> {
        self.shared.start()?;
        let captures = self
            .shared
            .running
            .lock()
            .as_ref()
            .map(|r| r.captures.clone())
            .ok_or("the browser is not running")?;
        let (tx, rx) = mpsc::channel();
        captures
            .send(tx)
            .map_err(|_| "the browser closed".to_string())?;
        let capture = rx
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| "the capture did not finish".to_string())??;
        self.shared.state.lock().latest = Some(capture.clone());
        (self.shared.changed)();
        Ok(capture)
    }

    /// A turn started or ended. Its end lets held OMP traffic through.
    pub fn set_turn(&self, active: bool) {
        let mut state = self.shared.state.lock();
        state.turn_active = active;
        if !active {
            state.preempted = false;
        }
        drop(state);
        (self.shared.changed)();
    }

    /// The person lets the agent use the browser again.
    pub fn resume(&self) {
        self.shared.state.lock().preempted = false;
        (self.shared.changed)();
    }
}

impl Drop for BrowserHost {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        self.shared.running.lock().take();
    }
}

impl Shared {
    /// The running Chromium's debugging port, starting it if needed. The
    /// launch runs outside every lock, so dropping the host never waits on
    /// it; a launch that finishes after the host closed closes its Chromium.
    fn start(self: &Arc<Self>) -> Result<u16, String> {
        let mut starting = self.starting.lock();
        loop {
            if self.closed() {
                return Err("the browser is closed".to_string());
            }
            if let Some(r) = self.running.lock().as_ref() {
                return Ok(r.chromium.port);
            }
            if !*starting {
                break;
            }
            self.launched.wait(&mut starting);
        }
        *starting = true;
        drop(starting);
        let result = self
            .exe
            .clone()
            .ok_or_else(|| "no Chrome or Chromium found; set CEDIAN_CHROMIUM".to_string())
            .and_then(|exe| Chromium::launch(&exe, &self.profile))
            .and_then(|chromium| {
                let captures = monitor::watch(chromium.port, self.clone())?;
                Ok(Running { chromium, captures })
            });
        let mut starting = self.starting.lock();
        *starting = false;
        let outcome = match result {
            Ok(_) if self.closed() => Err("the browser is closed".to_string()),
            Ok(r) => {
                let port = r.chromium.port;
                *self.running.lock() = Some(r);
                let mut state = self.state.lock();
                state.running = true;
                state.error = None;
                Ok(port)
            }
            Err(e) => {
                self.state.lock().error = Some(e.clone());
                Err(e)
            }
        };
        self.launched.notify_all();
        drop(starting);
        (self.changed)();
        outcome
    }

    /// OMP sent `Input.*` command `id` on `connection`.
    fn agent_input_sent(&self, connection: u64, id: u64) {
        let now = now_ms();
        let mut state = self.state.lock();
        state
            .agent_inputs
            .retain(|i| i.answered.is_none_or(|at| now - at < INPUT_KEEP_MS));
        state.agent_inputs.push(AgentInput {
            connection,
            id,
            sent: now,
            answered: None,
        });
    }

    /// Chromium answered message `id` on `connection`.
    fn agent_input_answered(&self, connection: u64, id: u64) {
        let mut state = self.state.lock();
        if let Some(input) = state
            .agent_inputs
            .iter_mut()
            .find(|i| i.connection == connection && i.id == id && i.answered.is_none())
        {
            input.answered = Some(now_ms());
        }
    }

    fn has_agent_input_in_flight(&self, connection: u64) -> bool {
        self.state
            .lock()
            .agent_inputs
            .iter()
            .any(|i| i.connection == connection && i.answered.is_none())
    }

    /// A trusted input event in the page, made at wall-clock `at` ms. It is
    /// the agent's when an agent `Input.*` command was in flight then;
    /// otherwise it is the person's.
    fn person_input(&self, at: Option<f64>) {
        let at = at.unwrap_or_else(now_ms);
        let mut state = self.state.lock();
        let agents = state.agent_inputs.iter().any(|i| {
            i.sent - INPUT_SLACK_MS <= at && at <= i.answered.unwrap_or(f64::MAX) + INPUT_SLACK_MS
        });
        if agents || !state.turn_active || state.preempted {
            return;
        }
        state.preempted = true;
        drop(state);
        (self.changed)();
    }

    fn held(&self) -> bool {
        let state = self.state.lock();
        state.preempted && state.turn_active
    }

    fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

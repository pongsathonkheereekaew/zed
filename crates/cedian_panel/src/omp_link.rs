//! The app's line to OMP (S9 U3, U4): one OMP process per workspace, started
//! the way the CLI starts it (`cedian_shell::launch`, the user's
//! `cedian.toml`, the spawn profile) on its own thread, so a dying OMP never
//! takes the IDE with it. Sessions live where OMP's CLI keeps them for the
//! project, so a session started in either opens in the other (ADR-0040
//! decision 5) and a restart's `open_session` adopts it; cedian's overlay
//! and audit log live in the workspace's state dir (ADR-0044).
//!
//! The link is cedian's gate for the app (§64): every tool execution and
//! every dialog outcome is a row in `audit.jsonl`. A person's answer is sent
//! and recorded in one step; a dialog still open when OMP dies, withdraws it
//! or is stopped is recorded as `abstain` (§63 lease, ADR-0013).

use cedian_omp::{
    DialogRecord, NewSession, OmpBinary, OmpRuntime, RouterEvent, RuntimeConfig, RuntimeControl,
    Sessions, SpawnPolicy, UserAnswer,
};
use cedian_shell::audit::AuditLog;
use cedian_shell::{Policy, RunKind};
use collections::HashMap;
use futures::channel::mpsc::UnboundedSender;
use omp_rpc::{ExtensionUiRequest, ExtensionUiResponse, HostUri, ImageContent};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

/// What the OMP thread tells the panel.
#[derive(Debug, Clone)]
pub enum LinkEvent {
    /// OMP is up and its session is open.
    Ready {
        session_id: String,
        /// The session's file in OMP's store, where the CLI finds it too.
        session_file: Option<String>,
        resumed: bool,
        /// Set under `policy = "omp"` (ADR-0035): OMP's config decides.
        policy_note: Option<String>,
    },
    /// One of OMP's events, `Disconnected` included.
    Event(RouterEvent),
    /// OMP could not start, or its session could not open.
    Failed(String),
    /// An audit row could not be written (ADR-0035: an unaudited turn fails).
    AuditFailed(String),
    /// Another process drives the session, or whether one does could not be
    /// checked: the app sends it no prompt (ADR-0040 decision 5).
    Taken { session_id: String, reason: String },
    /// The prompt was stopped before OMP got it.
    PromptCancelled,
    /// OMP ran the prompt, it was stopped, and the call ended in an error
    /// instead of a result.
    PromptStopped(String),
    /// OMP did not run the prompt: it refused it, or the call failed.
    PromptFailed(String),
    /// Stop's abort did not reach OMP.
    AbortFailed(String),
    /// OMP's answer to a Steer on subagent `id`.
    SubagentSteered {
        id: String,
        result: Result<(), String>,
    },
    /// OMP's answer to a Cancel on subagent `id`: whether it was running.
    SubagentCancelled {
        id: String,
        result: Result<bool, String>,
    },
}

/// What the panel asks of the OMP thread, in order.
enum Command {
    Prompt(Prompt),
    /// Check the session again for another driver.
    Retry,
    /// Leave the session to its driver and start a fresh one.
    NewSession,
    /// Park the thread until the sender is dropped, so a test can pin a
    /// prompt in the queue across a Restart.
    #[cfg(any(test, feature = "test-support"))]
    Hold(mpsc::Receiver<()>),
}

/// One prompt from the composer.
#[derive(Debug, Clone)]
pub struct Prompt {
    pub text: String,
    pub images: Vec<ImageContent>,
}

/// Everything one launch needs, resolved before any process starts.
pub struct LaunchSpec {
    pub binary: PathBuf,
    pub workdir: PathBuf,
    /// The workspace's state dir: the spawn overlay (`omp/`) and the audit log.
    pub state_dir: PathBuf,
    pub sessions: Sessions,
    pub policy: SpawnPolicy,
    pub policy_note: Option<String>,
    /// OMP's state root, where session owner leases live.
    pub omp_state: Option<PathBuf>,
    /// The host URI schemes registered before the session opens.
    pub uris: Vec<HostUri>,
}

impl LaunchSpec {
    /// Resolve settings, policy and paths for `workdir`. The app registers no
    /// host tools yet; the panel adds its `cedian://` scheme to `uris`.
    pub fn resolve(workdir: &Path) -> Result<Self, String> {
        let settings = cedian_shell::resolve_settings(workdir).map_err(|e| e.to_string())?;
        let binary = cedian_shell::launch::omp_binary()?;
        let choice = settings.policy_for(workdir, RunKind::Interactive);
        let mut policy = cedian_shell::launch::spawn_policy(&settings, choice.policy, &[]);
        let policy_note = match choice.policy {
            Policy::Cedian => {
                policy.config_allows = cedian_shell::launch::config_allows(&binary, workdir)?;
                None
            }
            Policy::Omp => Some("policy = \"omp\": OMP's own config decides approvals".to_string()),
        };
        Ok(Self {
            binary,
            workdir: workdir.to_path_buf(),
            state_dir: cedian_shell::state::dir(workdir)?,
            sessions: Sessions::OmpDefault,
            policy,
            policy_note,
            omp_state: cedian_omp::driver::state_root(),
            uris: Vec::new(),
        })
    }
}

/// A running OMP. Dropping it closes the dialogs OMP still waits on (a
/// cancel reply and an `abstain` row each), aborts a running turn so the
/// old process ends promptly, and shuts OMP down (no `Disconnected` follows).
pub struct OmpLink {
    commands: mpsc::Sender<Command>,
    pid: Arc<AtomicU32>,
    gate: Arc<Gate>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// A dropped link's OMP thread and process, which the next link waits on.
struct Previous {
    thread: std::thread::JoinHandle<()>,
    pid: Option<u32>,
}

/// How long a restart waits for the previous OMP to let go of the session
/// before it kills it, in milliseconds.
static PREVIOUS_EXIT_MS: AtomicU64 = AtomicU64::new(10_000);

/// How long the killed previous OMP's thread may take to wind down.
const KILLED_EXIT: Duration = Duration::from_secs(3);

/// Shorten how long a restart waits before killing the previous OMP.
#[cfg(any(test, feature = "test-support"))]
pub fn set_previous_exit(limit: Duration) {
    PREVIOUS_EXIT_MS.store(limit.as_millis() as u64, Ordering::Relaxed);
}

/// Why [`OmpLink::answer`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerError {
    /// OMP did not get the answer; the dialog stays open.
    NotSent(String),
    /// OMP got the answer, so the dialog is closed, but its audit row could
    /// not be written: the turn must fail (ADR-0035).
    Unaudited(String),
}

/// Shared by the panel (answers), the event thread (tool rows, dialogs
/// opening and closing) and the OMP thread (control, once spawned).
struct Gate {
    /// Taken back before OMP shuts down: a clone left here would keep its
    /// client, and so the process, alive.
    control: Mutex<Option<RuntimeControl>>,
    turn: Mutex<TurnState>,
    state: Mutex<GateState>,
    events: UnboundedSender<LinkEvent>,
}

/// The one prompt in flight, shared by the panel (send, Stop, Drop), the OMP
/// thread (which sends it) and the event thread (which sees it start), so a
/// Stop or Drop at any point ends it exactly once: before OMP has it, the
/// OMP thread drops it; once OMP started it, one abort goes out.
#[derive(Default)]
struct TurnState {
    phase: Phase,
    /// Stop or an audit failure: the current prompt must not run.
    cancelled: bool,
    /// The link was dropped: no further command runs.
    closed: bool,
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    #[default]
    Idle,
    /// Waiting in the queue or in the driver check.
    Queued,
    /// Written to OMP, not started yet.
    Sent,
    Started,
}

impl Gate {
    fn send(&self, reply: ExtensionUiResponse) -> Result<(), String> {
        let control = self.control.lock().clone().ok_or("OMP is not running")?;
        control.respond(reply).map_err(|e| e.to_string())
    }

    /// Cancel the current prompt; with `close`, every later one too.
    fn cancel(&self, close: bool) {
        let mut turn = self.turn.lock();
        let first = !std::mem::replace(&mut turn.cancelled, true);
        turn.closed |= close;
        if first && turn.phase == Phase::Started {
            self.abort();
        }
    }

    /// OMP started the prompt it was sent: abort it at once if cancelled.
    fn started(&self) {
        let mut turn = self.turn.lock();
        if turn.phase == Phase::Sent {
            turn.phase = Phase::Started;
            if turn.cancelled {
                self.abort();
            }
        }
    }

    fn abort(&self) {
        let Some(control) = self.control.lock().clone() else {
            return;
        };
        let events = self.events.clone();
        std::thread::spawn(move || {
            if let Err(e) = control.abort() {
                let _ = events.unbounded_send(LinkEvent::AbortFailed(e.to_string()));
            }
        });
    }
}

#[derive(Default)]
struct GateState {
    audit: Option<AuditLog>,
    /// Dialogs OMP is waiting on, by request id.
    open: HashMap<String, ExtensionUiRequest>,
}

impl GateState {
    fn record(&mut self, record: DialogRecord) -> Result<(), String> {
        match &mut self.audit {
            Some(audit) => audit.dialog(&record),
            None => Err("the audit log is not open".to_string()),
        }
    }

    /// Every open dialog, closed with nobody to answer it.
    fn abstain_all(&mut self) -> Result<(), String> {
        let open: Vec<_> = self.open.drain().map(|(_, request)| request).collect();
        open.iter()
            .filter_map(cedian_omp::abstained)
            .try_for_each(|record| self.record(record))
    }

    fn observe(&mut self, event: &RouterEvent) -> Result<(), String> {
        match event {
            RouterEvent::UiRequest(ExtensionUiRequest::Cancel(cancel)) => {
                match self.open.remove(&cancel.target_id) {
                    Some(request) => {
                        cedian_omp::abstained(&request).map_or(Ok(()), |record| self.record(record))
                    }
                    None => Ok(()),
                }
            }
            RouterEvent::UiRequest(request) => {
                if let Some((id, _)) = cedian_omp::dialog::dialog(request) {
                    self.open.insert(id.to_string(), request.clone());
                }
                Ok(())
            }
            RouterEvent::Disconnected => self.abstain_all(),
            event => match &mut self.audit {
                Some(audit) => audit.record(event),
                None => Ok(()),
            },
        }
    }

    fn answer(
        &mut self,
        request_id: &str,
        answer: UserAnswer,
        send: impl FnOnce(ExtensionUiResponse) -> Result<(), String>,
    ) -> Result<(), AnswerError> {
        let request = self.open.get(request_id).ok_or_else(|| {
            AnswerError::NotSent("OMP no longer waits on that dialog".to_string())
        })?;
        let (reply, record) = cedian_omp::user_answer(request, answer).ok_or_else(|| {
            AnswerError::NotSent("that answer does not fit the dialog".to_string())
        })?;
        send(reply).map_err(AnswerError::NotSent)?;
        self.open.remove(request_id);
        self.record(record).map_err(AnswerError::Unaudited)
    }

    /// Close dialog `request_id` (every open one when `None`) for OMP: a
    /// cancel reply, then its `abstain` row. A reply that fails to go out
    /// still leaves the row: nobody answered. Returns whether any was open.
    fn abandon(
        &mut self,
        request_id: Option<&str>,
        timed_out: bool,
        send: impl Fn(ExtensionUiResponse) -> Result<(), String>,
    ) -> Result<bool, String> {
        let requests: Vec<_> = match request_id {
            Some(id) => self.open.remove(id).into_iter().collect(),
            None => self.open.drain().map(|(_, request)| request).collect(),
        };
        let any = !requests.is_empty();
        let mut first_error = None;
        for request in &requests {
            if let Some((reply, record)) = cedian_omp::abandoned(request, timed_out) {
                if let Err(e) = send(reply) {
                    log::warn!("cedian: OMP did not get the dialog's cancel: {e}");
                }
                if let Err(e) = self.record(record) {
                    first_error.get_or_insert(e);
                }
            }
        }
        first_error.map_or(Ok(any), Err)
    }
}

impl OmpLink {
    /// Start OMP on its own thread; everything it reports goes to `events`.
    /// Prompts sent before it is ready wait in order.
    /// `previous`, the link this one replaces, is dropped here; the new OMP
    /// opens its session only once the old one has ended.
    pub fn start(
        spec: LaunchSpec,
        events: UnboundedSender<LinkEvent>,
        previous: Option<OmpLink>,
    ) -> Self {
        let previous = previous.and_then(|mut link| {
            let pid = link.pid();
            link.thread.take().map(|thread| Previous { thread, pid })
        });
        let (commands, command_rx) = mpsc::channel::<Command>();
        let pid = Arc::new(AtomicU32::new(0));
        let gate = Arc::new(Gate {
            control: Mutex::default(),
            turn: Mutex::default(),
            state: Mutex::default(),
            events: events.clone(),
        });
        let thread_pid = Arc::clone(&pid);
        let thread_gate = Arc::clone(&gate);
        let thread = std::thread::Builder::new()
            .name("cedian-omp".to_string())
            .spawn(move || run(spec, command_rx, events, thread_pid, thread_gate, previous))
            .inspect_err(|e| log::error!("cedian: cannot start the OMP thread: {e}"))
            .ok();
        Self {
            commands,
            pid,
            gate,
            thread,
        }
    }

    /// Send a prompt. One runs at a time: the panel sends the next only
    /// once this one has settled, failed, been refused or cancelled.
    pub fn send(&self, prompt: Prompt) -> Result<(), String> {
        {
            let mut turn = self.gate.turn.lock();
            turn.phase = Phase::Queued;
            turn.cancelled = false;
        }
        self.command(Command::Prompt(prompt))
    }

    /// Stop the current prompt: dropped if OMP does not have it yet,
    /// aborted if it does.
    pub fn cancel(&self) {
        self.gate.cancel(false);
    }

    /// Park the OMP thread before the next command; dropping the sender
    /// lets it go on.
    #[cfg(any(test, feature = "test-support"))]
    pub fn hold(&self) -> Result<mpsc::Sender<()>, String> {
        let (release, held) = mpsc::channel();
        self.command(Command::Hold(held)).map(|()| release)
    }

    /// Whether the current prompt waits in the queue or the driver check.
    #[cfg(any(test, feature = "test-support"))]
    pub fn prompt_queued(&self) -> bool {
        self.gate.turn.lock().phase == Phase::Queued
    }

    /// Whether OMP still waits on dialog `request_id`.
    pub fn is_open(&self, request_id: &str) -> bool {
        self.gate.state.lock().open.contains_key(request_id)
    }

    /// Make every later audit row fail, as a full disk would.
    #[cfg(any(test, feature = "test-support"))]
    pub fn break_audit(&self) {
        self.gate.state.lock().audit = None;
    }

    /// Check a taken session again; `Ready` or `Taken` follows.
    pub fn retry(&self) -> Result<(), String> {
        self.command(Command::Retry)
    }

    /// Start a fresh session; `Ready` follows.
    pub fn new_session(&self) -> Result<(), String> {
        self.command(Command::NewSession)
    }

    fn command(&self, command: Command) -> Result<(), String> {
        self.commands
            .send(command)
            .map_err(|_| "OMP is not running".to_string())
    }

    /// OMP's process id, once it has started.
    pub fn pid(&self) -> Option<u32> {
        Some(self.pid.load(Ordering::Relaxed)).filter(|pid| *pid != 0)
    }

    /// Steer subagent `id` off the UI thread; the answer comes back as
    /// [`LinkEvent::SubagentSteered`].
    pub fn steer_subagent(&self, id: String, message: String) {
        let not_running = LinkEvent::SubagentSteered {
            id: id.clone(),
            result: Err("OMP is not running".to_string()),
        };
        self.off_thread(not_running, move |control, _| LinkEvent::SubagentSteered {
            result: control
                .steer_subagent(&id, &message)
                .map_err(|e| e.to_string()),
            id,
        });
    }

    /// Cancel subagent `id` off the UI thread. The person's cancel is an
    /// audit row once OMP has answered (ADR-0050 decision 4).
    pub fn cancel_subagent(&self, id: String) {
        let not_running = LinkEvent::SubagentCancelled {
            id: id.clone(),
            result: Err("OMP is not running".to_string()),
        };
        self.off_thread(not_running, move |control, gate| {
            let result = control.cancel_subagent(&id).map_err(|e| e.to_string());
            if let Ok(cancelled) = result {
                let recorded = match &mut gate.state.lock().audit {
                    Some(audit) => audit.subagent_cancel(&id, cancelled),
                    None => Err("the audit log is not open".to_string()),
                };
                if let Err(e) = recorded {
                    let _ = gate.events.unbounded_send(LinkEvent::AuditFailed(e));
                }
            }
            LinkEvent::SubagentCancelled { id, result }
        });
    }

    /// Run a blocking control call on its own thread, as Stop's abort runs.
    fn off_thread(
        &self,
        not_running: LinkEvent,
        call: impl FnOnce(RuntimeControl, &Gate) -> LinkEvent + Send + 'static,
    ) {
        let gate = Arc::clone(&self.gate);
        let Some(control) = gate.control.lock().clone() else {
            let _ = gate.events.unbounded_send(not_running);
            return;
        };
        std::thread::spawn(move || {
            let event = call(control, &gate);
            let _ = gate.events.unbounded_send(event);
        });
    }

    /// Mid-turn control (abort, steer), once OMP has started. Its calls
    /// block until OMP answers: run them off the UI thread.
    pub fn control(&self) -> Option<RuntimeControl> {
        self.gate.control.lock().clone()
    }

    /// Send a person's answer to dialog `request_id` and record it.
    pub fn answer(&self, request_id: &str, answer: UserAnswer) -> Result<(), AnswerError> {
        self.gate
            .state
            .lock()
            .answer(request_id, answer, |reply| self.gate.send(reply))
    }

    /// Stop: close every dialog OMP waits on. The caller aborts the turn.
    pub fn close_dialogs(&self) -> Result<(), String> {
        self.gate
            .state
            .lock()
            .abandon(None, false, |reply| self.gate.send(reply))
            .map(|_| ())
    }

    /// The §63 lease on dialog `request_id` ran out: OMP gets a timed-out
    /// cancel. `Ok(false)` when it was already closed.
    pub fn expire(&self, request_id: &str) -> Result<bool, String> {
        self.gate
            .state
            .lock()
            .abandon(Some(request_id), true, |reply| self.gate.send(reply))
    }
}

impl Drop for OmpLink {
    fn drop(&mut self) {
        if let Err(e) = self.close_dialogs() {
            log::error!("cedian: {e}");
        }
        self.gate.cancel(true);
    }
}

fn run(
    spec: LaunchSpec,
    commands: mpsc::Receiver<Command>,
    events: UnboundedSender<LinkEvent>,
    pid: Arc<AtomicU32>,
    gate: Arc<Gate>,
    previous: Option<Previous>,
) {
    match AuditLog::open(&spec.state_dir, spec.policy.approvals) {
        Ok(audit) => gate.state.lock().audit = Some(audit),
        Err(e) => {
            let _ = events.unbounded_send(LinkEvent::Failed(format!(
                "OMP not started, nothing could be audited: {e}"
            )));
            return;
        }
    }
    let mut runtime = match OmpRuntime::spawn(RuntimeConfig {
        binary: OmpBinary::Bundled(spec.binary),
        session_dir: spec.state_dir.join("omp"),
        sessions: spec.sessions,
        cwd: spec.workdir,
        ask_dialog: true,
        prompt_timeout: Duration::from_secs(600),
        policy: spec.policy,
    }) {
        Ok(runtime) => runtime,
        Err(e) => {
            let _ = events.unbounded_send(LinkEvent::Failed(format!("OMP did not start: {e}")));
            return;
        }
    };
    if !spec.uris.is_empty()
        && let Err(e) = runtime.set_host_uris(spec.uris)
    {
        let _ = events.unbounded_send(LinkEvent::Failed(format!(
            "OMP refused cedian:// context: {e}"
        )));
        let own = runtime.pid();
        shutdown(runtime, &gate, own);
        return;
    }
    if let Err(e) = runtime.subscribe_subagents() {
        let _ = events.unbounded_send(LinkEvent::Failed(format!(
            "OMP refused the subagent subscription: {e}"
        )));
        let own = runtime.pid();
        shutdown(runtime, &gate, own);
        return;
    }
    let own = runtime.pid();
    pid.store(own.unwrap_or(0), Ordering::Relaxed);
    *gate.control.lock() = Some(runtime.control());
    let (_sub, router_events) = runtime.router().subscribe();
    let forward = events.clone();
    let forward_gate = Arc::clone(&gate);
    std::thread::spawn(move || {
        for event in router_events {
            if matches!(event, RouterEvent::AgentStart) {
                forward_gate.started();
            }
            if let Err(e) = forward_gate.state.lock().observe(&event) {
                let _ = forward.unbounded_send(LinkEvent::AuditFailed(e));
            }
            if forward.unbounded_send(LinkEvent::Event(event)).is_err() {
                break;
            }
        }
    });
    if let Some(previous) = previous {
        wait_for(previous);
    }
    let mut session = match runtime.open_session("app") {
        Ok(opened) => Session {
            id: opened.session_id,
            file: opened.session_file,
            resumed: opened.resumed,
        },
        Err(e) => {
            let _ = events.unbounded_send(LinkEvent::Failed(format!(
                "OMP's session did not open: {e}"
            )));
            shutdown(runtime, &gate, own);
            return;
        }
    };
    let state = spec.omp_state;
    let taken = |session: &Session| -> Option<LinkEvent> {
        let files = cedian_omp::driver::session_files(
            state.as_deref(),
            &session.id,
            session.file.as_deref().map(Path::new),
        );
        let reason = match cedian_omp::driver::other_drivers_of(&files, own, session.resumed) {
            Ok(others) if others.is_empty() => return None,
            Ok(others) => format!(
                "another process drives this session (pid {})",
                others
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Err(e) => format!("cannot check whether another process drives this session: {e}"),
        };
        Some(LinkEvent::Taken {
            session_id: session.id.clone(),
            reason,
        })
    };
    let ready = |session: &Session| LinkEvent::Ready {
        session_id: session.id.clone(),
        session_file: session.file.clone(),
        resumed: session.resumed,
        policy_note: spec.policy_note.clone(),
    };
    let first = if session.resumed {
        taken(&session)
    } else {
        None
    };
    let _ = events.unbounded_send(first.unwrap_or_else(|| ready(&session)));
    let idle = |turn: &mut TurnState| turn.phase = Phase::Idle;
    for command in commands {
        if gate.turn.lock().closed {
            break;
        }
        match command {
            Command::Prompt(prompt) => {
                if let Some(refusal) = taken(&session) {
                    idle(&mut gate.turn.lock());
                    let _ = events.unbounded_send(refusal);
                    continue;
                }
                {
                    let mut turn = gate.turn.lock();
                    if turn.cancelled || turn.closed {
                        idle(&mut turn);
                        let _ = events.unbounded_send(LinkEvent::PromptCancelled);
                        continue;
                    }
                    turn.phase = Phase::Sent;
                }
                let result = runtime.prompt(&prompt.text, prompt.images);
                let (cancelled, started) = {
                    let mut turn = gate.turn.lock();
                    let started = turn.phase == Phase::Started;
                    idle(&mut turn);
                    (turn.cancelled, started)
                };
                if let Err(e) = result {
                    let _ = events.unbounded_send(match (cancelled, started) {
                        (true, true) => LinkEvent::PromptStopped(e.to_string()),
                        (true, false) => LinkEvent::PromptCancelled,
                        (false, _) => LinkEvent::PromptFailed(e.to_string()),
                    });
                }
            }
            Command::Retry => {
                let _ = events.unbounded_send(taken(&session).unwrap_or_else(|| ready(&session)));
            }
            Command::NewSession => {
                let refused = |reason: String| LinkEvent::Taken {
                    session_id: session.id.clone(),
                    reason,
                };
                let event = match runtime.new_session(None, "app") {
                    Ok(NewSession::Started(state)) => {
                        session = Session {
                            id: state.session_id,
                            file: state.session_file,
                            resumed: false,
                        };
                        ready(&session)
                    }
                    Ok(NewSession::Unknown(e)) => LinkEvent::Failed(format!(
                        "OMP started a new session but did not say which: {e}"
                    )),
                    Ok(NewSession::Declined) => {
                        refused("OMP declined to start a new session".to_string())
                    }
                    Err(e) => refused(format!("a new session did not start: {e}")),
                };
                let _ = events.unbounded_send(event);
            }
            #[cfg(any(test, feature = "test-support"))]
            Command::Hold(held) => {
                let _ = held.recv();
            }
        }
    }
    shutdown(runtime, &gate, own);
}

/// Shut OMP down; if a control call still holds its client, kill it, so
/// this thread ending means the process no longer holds the session.
fn shutdown(runtime: OmpRuntime, gate: &Gate, own: Option<u32>) {
    gate.control.lock().take();
    if let Err(e) = runtime.shutdown() {
        log::warn!("cedian: OMP did not shut down ({e}); killing it");
        if let Some(pid) = own {
            cedian_omp::driver::kill_group(pid);
        }
    }
}

/// Wait for the previous link's OMP to end, so it no longer holds the
/// session; it is ours, so one that does not end in time is killed with its
/// whole process group, and its thread gets a little longer to wind down.
fn wait_for(previous: Previous) {
    let limit = Duration::from_millis(PREVIOUS_EXIT_MS.load(Ordering::Relaxed));
    if finished_within(&previous.thread, limit) {
        return;
    }
    let Some(pid) = previous.pid else {
        // No pid to kill yet: still wait for the thread rather than open the
        // session under it and meet Taken.
        log::warn!("cedian: the previous OMP did not exit and has no pid yet; waiting");
        if !finished_within(&previous.thread, limit + KILLED_EXIT) {
            log::error!("cedian: the previous OMP's thread did not end");
        }
        return;
    };
    log::warn!("cedian: the previous OMP did not exit; killing it");
    cedian_omp::driver::kill_group(pid);
    if !finished_within(&previous.thread, KILLED_EXIT) {
        log::error!("cedian: the previous OMP's thread did not end after the kill");
    }
}

fn finished_within(thread: &std::thread::JoinHandle<()>, limit: Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    while !thread.is_finished() {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

struct Session {
    id: String,
    file: Option<String>,
    resumed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;
    use omp_rpc::wire::CancelUiResponse;

    fn approval(id: &str) -> RouterEvent {
        RouterEvent::UiRequest(
            ExtensionUiRequest::from_value(serde_json::json!({
                "type": "extension_ui_request", "method": "select", "id": id,
                "title": "Allow tool: bash\nCommand: ls", "options": ["Approve", "Deny"]
            }))
            .unwrap(),
        )
    }

    fn gate(dir: &Path) -> GateState {
        GateState {
            audit: Some(AuditLog::open(dir, SpawnPolicy::default().approvals).unwrap()),
            open: HashMap::default(),
        }
    }

    fn rows(dir: &Path) -> Vec<(String, String)> {
        std::fs::read_to_string(dir.join(cedian_shell::audit::AUDIT_FILE))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["item"].clone())
            .filter(|item| item["kind"] == "gate")
            .map(|item| {
                (
                    item["decision"].as_str().unwrap().to_string(),
                    item["answered_by"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cedian-gate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn abstain() -> (String, String) {
        ("abstain".to_string(), "cedian".to_string())
    }

    #[test]
    fn omp_withdrawing_a_dialog_is_one_abstain_row() {
        let dir = temp("cancel");
        let mut gate = gate(&dir);
        gate.observe(&approval("d1")).unwrap();
        let cancel = RouterEvent::UiRequest(
            ExtensionUiRequest::from_value(serde_json::json!({
                "type": "extension_ui_request", "method": "cancel", "id": "c1", "targetId": "d1"
            }))
            .unwrap(),
        );
        gate.observe(&cancel).unwrap();
        gate.observe(&cancel).unwrap();
        assert!(gate.open.is_empty());
        assert_eq!(rows(&dir), [abstain()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn omp_dying_with_a_dialog_open_is_one_abstain_row() {
        let dir = temp("disconnect");
        let mut gate = gate(&dir);
        gate.observe(&approval("d1")).unwrap();
        gate.observe(&RouterEvent::Disconnected).unwrap();
        gate.observe(&RouterEvent::Disconnected).unwrap();
        assert!(gate.open.is_empty());
        assert_eq!(rows(&dir), [abstain()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_answer_omp_got_but_the_audit_lost_closes_the_dialog_and_fails_the_turn() {
        let mut gate = GateState::default();
        gate.observe(&approval("d1")).unwrap();
        let sent = std::cell::RefCell::new(Vec::new());
        let result = gate.answer("d1", UserAnswer::Choice("Approve".into()), |reply| {
            sent.borrow_mut().push(reply);
            Ok(())
        });
        assert!(
            matches!(result, Err(AnswerError::Unaudited(_))),
            "{result:?}"
        );
        assert_eq!(sent.borrow().len(), 1, "OMP got the answer");
        assert!(gate.open.is_empty(), "nothing left to answer twice");
    }

    #[test]
    fn an_answer_omp_did_not_get_keeps_the_dialog_open() {
        let dir = temp("unsent");
        let mut gate = gate(&dir);
        gate.observe(&approval("d1")).unwrap();
        let result = gate.answer("d1", UserAnswer::Choice("Approve".into()), |_| {
            Err("pipe closed".to_string())
        });
        assert_eq!(result, Err(AnswerError::NotSent("pipe closed".to_string())));
        assert!(gate.open.contains_key("d1"));
        assert!(rows(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dialog_cedian_closes_gets_a_cancel_and_an_abstain_row() {
        let dir = temp("abandon");
        let mut gate = gate(&dir);
        gate.observe(&approval("d1")).unwrap();
        let sent = std::cell::RefCell::new(Vec::new());
        let send = |reply| {
            sent.borrow_mut().push(reply);
            Ok(())
        };
        assert_eq!(gate.abandon(Some("d1"), true, send), Ok(true));
        assert_eq!(gate.abandon(Some("d1"), true, send), Ok(false));
        assert_eq!(
            *sent.borrow(),
            [ExtensionUiResponse::CancelUiResponse(CancelUiResponse {
                id: "d1".into(),
                cancelled: omp_rpc::wire::LitTrue,
                timed_out: Some(true),
            })]
        );
        assert_eq!(rows(&dir), [abstain()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn closing_every_dialog_with_the_audit_failing_still_cancels_each() {
        let mut gate = GateState::default();
        gate.observe(&approval("d1")).unwrap();
        gate.observe(&approval("d2")).unwrap();
        let sent = std::cell::RefCell::new(Vec::new());
        let result = gate.abandon(None, false, |reply| {
            sent.borrow_mut().push(reply);
            Ok(())
        });
        assert!(result.is_err(), "{result:?}");
        assert_eq!(sent.borrow().len(), 2, "every dialog got its cancel");
        assert!(gate.open.is_empty());
    }

    fn ready(
        events: &mut futures::channel::mpsc::UnboundedReceiver<LinkEvent>,
    ) -> (String, bool, Option<String>) {
        futures::executor::block_on(async {
            while let Some(event) = events.next().await {
                match event {
                    LinkEvent::Ready {
                        session_id,
                        resumed,
                        session_file,
                        ..
                    } => return (session_id, resumed, session_file),
                    LinkEvent::Failed(e) => panic!("{e}"),
                    _ => {}
                }
            }
            panic!("OMP thread ended without a session")
        })
    }

    /// Real OMP keeps the app's session in its own store, where the CLI
    /// finds it (ADR-0040 decision 5), and adopts it on restart. OMP writes a
    /// session only once it has a message, so this sends one short prompt
    /// (one model call). The session folder it made is removed after.
    /// `cargo test -p cedian_panel -- --ignored live_`
    #[test]
    #[ignore]
    fn live_restart_adopts_the_same_omp_session() {
        let root = std::env::temp_dir().join(format!("cedian-u3-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("ws")).unwrap();
        let root = root.canonicalize().unwrap();
        let spec = || LaunchSpec {
            binary: cedian_shell::launch::omp_binary().unwrap(),
            workdir: root.join("ws"),
            state_dir: root.join("state"),
            sessions: Sessions::OmpDefault,
            policy: SpawnPolicy::default(),
            policy_note: None,
            omp_state: cedian_omp::driver::state_root(),
            uris: Vec::new(),
        };
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let link = OmpLink::start(spec(), tx, None);
        let (first, resumed, file) = ready(&mut rx);
        assert!(!resumed);
        let file = PathBuf::from(file.expect("OMP names the session file"));
        let agent = cedian_omp::OmpConfig::new(spec().binary)
            .dir(&root)
            .unwrap();
        assert!(
            file.starts_with(agent.join("sessions")),
            "in OMP's own store: {}",
            file.display()
        );
        link.send(Prompt {
            text: "Reply with exactly: ok".to_string(),
            images: Vec::new(),
        })
        .unwrap();
        futures::executor::block_on(async {
            while let Some(event) = rx.next().await {
                if matches!(event, LinkEvent::Event(RouterEvent::Settled)) {
                    break;
                }
            }
        });
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let _link = OmpLink::start(spec(), tx, Some(link));
        let (again, resumed, _) = ready(&mut rx);
        assert_eq!((again.as_str(), resumed), (first.as_str(), true));
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }
}

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
    DialogRecord, OmpBinary, OmpRuntime, RouterEvent, RuntimeConfig, RuntimeControl, Sessions,
    SpawnPolicy, UserAnswer,
};
use cedian_shell::audit::AuditLog;
use cedian_shell::{Policy, RunKind};
use collections::HashMap;
use futures::channel::mpsc::UnboundedSender;
use omp_rpc::{ExtensionUiRequest, ExtensionUiResponse, ImageContent};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
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
}

impl LaunchSpec {
    /// Resolve settings, policy and paths for `workdir`. The app registers no
    /// host tools yet (U5 adds them).
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
        })
    }
}

/// A running OMP. Dropping it closes the dialogs OMP still waits on (a
/// cancel reply and an `abstain` row each), aborts a running turn so the
/// old process ends promptly, and shuts OMP down (no `Disconnected` follows).
pub struct OmpLink {
    prompts: mpsc::Sender<Prompt>,
    pid: Arc<AtomicU32>,
    gate: Arc<Gate>,
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
#[derive(Default)]
struct Gate {
    control: OnceLock<RuntimeControl>,
    /// Set while OMP runs a prompt.
    turn: AtomicBool,
    state: Mutex<GateState>,
}

impl Gate {
    fn send(&self, reply: ExtensionUiResponse) -> Result<(), String> {
        let control = self.control.get().ok_or("OMP is not running")?;
        control.respond(reply).map_err(|e| e.to_string())
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
        for request in &requests {
            if let Some((reply, record)) = cedian_omp::abandoned(request, timed_out) {
                if let Err(e) = send(reply) {
                    log::warn!("cedian: OMP did not get the dialog's cancel: {e}");
                }
                self.record(record)?;
            }
        }
        Ok(any)
    }
}

impl OmpLink {
    /// Start OMP on its own thread; everything it reports goes to `events`.
    /// Prompts sent before it is ready wait in order.
    pub fn start(spec: LaunchSpec, events: UnboundedSender<LinkEvent>) -> Self {
        let (prompts, prompt_rx) = mpsc::channel::<Prompt>();
        let pid = Arc::new(AtomicU32::new(0));
        let gate = Arc::new(Gate::default());
        let thread_pid = Arc::clone(&pid);
        let thread_gate = Arc::clone(&gate);
        let spawned = std::thread::Builder::new()
            .name("cedian-omp".to_string())
            .spawn(move || run(spec, prompt_rx, events, thread_pid, thread_gate));
        if let Err(e) = spawned {
            log::error!("cedian: cannot start the OMP thread: {e}");
        }
        Self { prompts, pid, gate }
    }

    pub fn send(&self, prompt: Prompt) -> Result<(), String> {
        self.prompts
            .send(prompt)
            .map_err(|_| "OMP is not running".to_string())
    }

    /// OMP's process id, once it has started.
    pub fn pid(&self) -> Option<u32> {
        Some(self.pid.load(Ordering::Relaxed)).filter(|pid| *pid != 0)
    }

    /// Mid-turn control (abort, steer), once OMP has started. Its calls
    /// block until OMP answers: run them off the UI thread.
    pub fn control(&self) -> Option<RuntimeControl> {
        self.gate.control.get().cloned()
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
        if self.gate.turn.load(Ordering::Relaxed)
            && let Some(control) = self.control()
        {
            std::thread::spawn(move || {
                if let Err(e) = control.abort() {
                    log::warn!("cedian: abort on shutdown: {e}");
                }
            });
        }
    }
}

fn run(
    spec: LaunchSpec,
    prompts: mpsc::Receiver<Prompt>,
    events: UnboundedSender<LinkEvent>,
    pid: Arc<AtomicU32>,
    gate: Arc<Gate>,
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
    pid.store(runtime.pid().unwrap_or(0), Ordering::Relaxed);
    let _ = gate.control.set(runtime.control());
    let (_sub, router_events) = runtime.router().subscribe();
    let forward = events.clone();
    let forward_gate = Arc::clone(&gate);
    std::thread::spawn(move || {
        for event in router_events {
            if let Err(e) = forward_gate.state.lock().observe(&event) {
                let _ = forward.unbounded_send(LinkEvent::AuditFailed(e));
            }
            if forward.unbounded_send(LinkEvent::Event(event)).is_err() {
                break;
            }
        }
    });
    match runtime.open_session("app") {
        Ok(session) => {
            let _ = events.unbounded_send(LinkEvent::Ready {
                session_id: session.session_id,
                session_file: session.session_file,
                resumed: session.resumed,
                policy_note: spec.policy_note,
            });
        }
        Err(e) => {
            let _ = events.unbounded_send(LinkEvent::Failed(format!(
                "OMP's session did not open: {e}"
            )));
            let _ = runtime.shutdown();
            return;
        }
    }
    for prompt in prompts {
        gate.turn.store(true, Ordering::Relaxed);
        if let Err(e) = runtime.prompt(&prompt.text, prompt.images) {
            log::error!("cedian: OMP turn failed: {e}");
        }
        gate.turn.store(false, Ordering::Relaxed);
    }
    let _ = runtime.shutdown();
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
                    LinkEvent::Event(_) | LinkEvent::AuditFailed(_) => {}
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
        };
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let link = OmpLink::start(spec(), tx);
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
        drop(link);
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let _link = OmpLink::start(spec(), tx);
        let (again, resumed, _) = ready(&mut rx);
        assert_eq!((again.as_str(), resumed), (first.as_str(), true));
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }
}

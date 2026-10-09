//! The cedian dock panel (S9a T2/T4, S9 U3): one prompt box, the streamed OMP
//! reply rendered from the headless `cedian_agent::Thread`, and agent-edit
//! import.
//!
//! The panel starts OMP when it opens on a folder ([`crate::omp_link`]): its
//! own thread, the user's `cedian.toml`, the spawn profile. Events cross into
//! GPUI over a channel; edit-class tool events drive [`crate::import`]. When
//! OMP dies the panel says why and offers Restart; the IDE keeps running.
//!
//! OMP's dialogs (approvals, `confirm`, `input`, `editor`, `ask`) are answered
//! here (U4, §63): several can be open at once, keyed by request id. A turn
//! can be stopped, and images pasted into the composer go with the prompt.
//!
//! Every imported agent transaction feeds the task's [`TaskReview`] (U5,
//! §18, ADR-0006). A file OMP is about to write that is not open is opened
//! first, so its write is a reviewable transaction too. The Review Changes
//! view lists each file's hunks with Accept and Reject, Accept all, and
//! Revert turn.

use crate::browser::BrowserHost;
use crate::context;
use crate::dialogs::OpenDialog;
use crate::import::{self, ImportOutcome, Mark};
use crate::omp_link::{AnswerError, LaunchSpec, LinkEvent, OmpLink, Prompt};
use crate::omp_settings::OmpSettings;
use crate::review::{ReviewError, TaskReview};
use crate::workflow_view::WorkflowView;
use cedian_agent::{Thread, ThreadEvent};
use cedian_agent_ui::{SubagentRow, SubagentTree, ToolCard};
use cedian_omp::{RouterEvent, SubagentStatus, TextBefore, UserAnswer};
use cedian_review::{HunkKey, HunkStatus};
use cedian_workflow::{CurrentState, Evidence, Gate, GateResult, Outcome, WorkflowStatus};
use cedian_workspace::{capture_ambient, render_snapshot};
use collections::{HashMap, HashSet, IndexMap};
use editor::{Editor, actions::Paste};
use futures::{StreamExt, channel::mpsc};
use gpui::{
    Action, AnyElement, App, AsyncWindowContext, ClipboardEntry, Context, ElementId, Entity,
    EntityId, EventEmitter, FocusHandle, Focusable, Pixels, Render, Subscription, Task, WeakEntity,
    Window, actions, px,
};
use language::{Buffer, BufferEvent};
use omp_rpc::{ExtensionUiRequest, ImageContent};

/// The gate every panel browser capture is evidence for.
pub const BROWSER_GATE: &str = "browser";
use project::Project;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use ui::{Button, Label, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    cedian_panel,
    [
        /// Toggle focus on the cedian panel.
        ToggleFocus
    ]
);

const PANEL_KEY: &str = "CedianPanel";
/// OMP tools that write the filesystem directly.
const EDIT_TOOLS: &[&str] = &["edit", "write", "ast_edit"];

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<CedianPanel>(window, cx);
        });
    })
    .detach();
}

/// One edit-class tool call in flight: the open buffers marked before its
/// write, and the files it names that are not open, with their disk text
/// at the tool's start (`None`: absent then). Those are opened only at the
/// tool's end, so the write is a hunk against that text however the open
/// and the write race (a new file is one all-added hunk).
struct CallMarks {
    marks: Vec<(Entity<Buffer>, Mark)>,
    unopened: Vec<(PathBuf, Option<String>)>,
    /// Disk reads of unopened files still in flight.
    reading: usize,
    /// `ToolEnd` came (with its `is_error`) before every read finished.
    ended: Option<bool>,
    /// Each file's text before the call as OMP's result reports it, by
    /// absolute path.
    before: Vec<(PathBuf, TextBefore)>,
    /// A `bash` call (ADR-0047): the files not open that changed while it
    /// ran, `(absolute, as the review names it)`.
    bash: Option<Vec<(PathBuf, PathBuf)>>,
    /// Marked buffers another mutating call also marked while one of the
    /// two was a `bash` call, with that call's id: which call wrote what is
    /// unknown, so neither imports them.
    overlapped: Vec<(Entity<Buffer>, String)>,
}

/// The tool whose writes cedian learns only from the files it changed.
const BASH_TOOL: &str = "bash";

/// How long a dialog waits on the person before cedian closes it (§63,
/// ADR-0013).
pub const DIALOG_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// The running turn, as the panel shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Turn {
    Idle,
    /// Sent; OMP has not started it yet.
    Queued,
    Streaming,
    /// Stop was pressed; OMP has not settled yet.
    Stopping,
    /// The turn failed and was aborted; OMP has not settled yet.
    Failed(String),
}

impl Turn {
    fn label(&self) -> String {
        match self {
            Turn::Idle => "idle".to_string(),
            Turn::Queued => "queued".to_string(),
            Turn::Streaming => "streaming".to_string(),
            Turn::Stopping => "stopping".to_string(),
            Turn::Failed(reason) => format!("failed: {reason}"),
        }
    }
}

/// Where the panel's OMP stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    /// No folder open yet, so no OMP.
    NotStarted,
    Starting,
    /// Retry asked OMP's thread to check the session for another driver
    /// again; `Ready` or `Taken` follows.
    Checking,
    Ready {
        session_id: String,
        resumed: bool,
        policy_note: Option<String>,
    },
    /// OMP failed to start or died. Prompts are refused until Restart.
    Stopped(String),
    /// Another process drives the session (ADR-0040 decision 5). Prompts
    /// are refused until Retry finds it free or a new session starts.
    Taken {
        session_id: String,
        reason: String,
    },
}

/// The app's task: its review, its workflow evidence and its correction rows.
pub const TASK_ID: &str = "panel";

pub struct CedianPanel {
    focus_handle: FocusHandle,
    project: Entity<Project>,
    input: Entity<Editor>,
    thread: Thread,
    /// OMP's subagents, each under the `task` call that started it.
    subagents: SubagentTree,
    /// Each running subagent's Steer text box, by id.
    steer_boxes: HashMap<String, Entity<Editor>>,
    /// What the last Steer or Cancel on a subagent row came to, by id.
    subagent_notes: HashMap<String, String>,
    link: Option<OmpLink>,
    connection: Connection,
    calls: HashMap<String, CallMarks>,
    review: TaskReview,
    /// The reviewed buffers whose edits rebuild the review (so render only
    /// shows what is there).
    watched: HashSet<EntityId>,
    buffer_subscriptions: Vec<Subscription>,
    show_review: bool,
    /// Accept all is waiting on whether to include the STALE hunks.
    confirm_accept_all: bool,
    turn: Turn,
    /// The last thing the person should know that is not on a dialog: a
    /// refused prompt, a failed turn, a dialog cedian closed.
    notice: Option<String>,
    /// The correction ledger's last failed write: one line, however many
    /// rows failed, so a ledger that keeps failing does not grow the notice.
    ledger_error: Option<String>,
    /// OMP's dialogs waiting on the person, by request id, oldest first.
    dialogs: IndexMap<String, OpenDialog>,
    /// Images pasted into the composer, sent with the next prompt.
    images: Vec<ImageContent>,
    /// The OMP settings page, shown instead of the thread when open.
    settings: Option<Entity<OmpSettings>>,
    show_settings: bool,
    /// The workspace whose active editor gives the selection OMP sees.
    workspace: Option<WeakEntity<Workspace>>,
    _events: Option<Task<()>>,
    /// The workspace's Chromium, shared with OMP (ADR-0049); closed with
    /// the panel.
    browser: Option<Arc<BrowserHost>>,
    _browser_events: Option<Task<()>>,
    _browser_quit: Option<Subscription>,
    /// Evidence from the panel's browser captures, each bound to its frame.
    browser_evidence: Vec<Evidence>,
    /// Answers OMP's `cedian://` reads.
    _context: Option<Task<()>>,
    /// The workspace's state dir (ADR-0044), once known.
    state_dir: Option<PathBuf>,
    /// The task's workflow as last read, when one was started.
    workflow: Option<WorkflowView>,
    _workflow_load: Option<Task<()>>,
    /// The §54 escalation the blocked workflow waits on the person for.
    escalation: Option<String>,
    _worktree_changes: Option<Subscription>,
}

/// How long a quitting app waits for Chromium to close itself (flushing its
/// cookies) before killing it. The quit hook blocks the main thread.
const BROWSER_QUIT_LIMIT: Duration = Duration::from_secs(2);

impl CedianPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            workspace.update_in(cx, |workspace, window, cx| {
                let project = workspace.project().clone();
                let handle = cx.entity().downgrade();
                cx.new(|cx| {
                    let mut panel = Self::new(project, window, cx);
                    panel.set_workspace(handle);
                    panel
                })
            })
        })
    }

    pub fn new(project: Entity<Project>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Ask OMP…", window, cx);
            editor
        });
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            project,
            input,
            thread: Thread::new(),
            subagents: SubagentTree::default(),
            steer_boxes: HashMap::default(),
            subagent_notes: HashMap::default(),
            link: None,
            connection: Connection::NotStarted,
            calls: HashMap::default(),
            review: TaskReview::new(TASK_ID),
            watched: HashSet::default(),
            buffer_subscriptions: Vec::new(),
            show_review: false,
            confirm_accept_all: false,
            turn: Turn::Idle,
            notice: None,
            ledger_error: None,
            dialogs: IndexMap::default(),
            images: Vec::new(),
            settings: None,
            show_settings: false,
            state_dir: None,
            workflow: None,
            _workflow_load: None,
            escalation: None,
            workspace: None,
            _events: None,
            browser: None,
            _browser_events: None,
            _browser_quit: None,
            browser_evidence: Vec::new(),
            _context: None,
            _worktree_changes: None,
        };
        this._worktree_changes = Some(cx.subscribe(&this.project, |this, project, event, cx| {
            if let project::Event::WorktreeUpdatedEntries(worktree, changes) = event {
                this.bash_changed(&project, *worktree, changes, cx);
            }
        }));
        if this.workspace_root(cx).is_some() {
            this.start(window, cx);
        }
        this
    }

    /// The workspace whose active editor and selection OMP is told about.
    pub fn set_workspace(&mut self, workspace: WeakEntity<Workspace>) {
        self.workspace = Some(workspace);
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn turn(&self) -> &Turn {
        &self.turn
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Put `text` in the prompt box, as typing would.
    pub fn set_prompt(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input
            .update(cx, |editor, cx| editor.set_text(text, window, cx));
    }

    /// Send the prompt box, as the Send button does.
    pub fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.send(window, cx);
    }

    /// The open dialogs' request ids, oldest first.
    pub fn dialog_ids(&self) -> Vec<String> {
        self.dialogs.keys().cloned().collect()
    }

    /// The thread as the panel renders it: messages, then tool cards, each
    /// followed by the subagents it started.
    pub fn transcript(&self) -> Vec<String> {
        let (messages, cards) = cedian_agent_ui::render_thread(self.thread.events());
        let mut lines: Vec<String> = messages
            .into_iter()
            .map(|m| format!("{:?}: {}", m.role, m.text))
            .collect();
        for entry in self.cards_with_subagents(cards) {
            match entry {
                Entry::Card(card) => lines.push(card_line(&card)),
                Entry::Subagent(row) => lines.push(subagent_line(row)),
            }
        }
        lines
    }

    pub fn subagents(&self) -> &SubagentTree {
        &self.subagents
    }

    pub fn subagent_note(&self, id: &str) -> Option<&str> {
        self.subagent_notes.get(id).map(String::as_str)
    }

    pub fn subagent_steer_box(&self, id: &str) -> Option<&Entity<Editor>> {
        self.steer_boxes.get(id)
    }

    /// The Steer button on subagent `id`'s row.
    pub fn steer_subagent(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.steer_boxes.get(id) else {
            return;
        };
        let text = input.read(cx).text(cx);
        if text.trim().is_empty() {
            return;
        }
        let Some(link) = &self.link else {
            self.subagent_notes
                .insert(id.to_string(), "OMP is not running".to_string());
            cx.notify();
            return;
        };
        link.steer_subagent(id.to_string(), text);
        input.update(cx, |editor, cx| editor.clear(window, cx));
        self.subagent_notes
            .insert(id.to_string(), "steering…".to_string());
        cx.notify();
    }

    /// The Cancel button on subagent `id`'s row.
    pub fn cancel_subagent(&mut self, id: &str, cx: &mut Context<Self>) {
        let note = match &self.link {
            Some(link) => {
                link.cancel_subagent(id.to_string());
                "cancelling…"
            }
            None => {
                let root = self.workspace_root(cx);
                let id = id.to_string();
                let task = cx.background_spawn(async move {
                    let root = root.ok_or_else(|| "no folder is open".to_string())?;
                    crate::omp_link::cancel_without_omp(&root, &id)
                });
                cx.spawn(async move |this, cx| {
                    if let Err(e) = task.await {
                        this.update(cx, |this, cx| {
                            this.audit_failed(e, cx);
                            cx.notify();
                        })
                        .ok();
                    }
                })
                .detach();
                "OMP is not running"
            }
        };
        self.subagent_notes.insert(id.to_string(), note.to_string());
        cx.notify();
    }

    /// Cards in order, each followed by its subagents; subagents whose card
    /// is not in the thread come last.
    fn cards_with_subagents(&self, cards: Vec<ToolCard>) -> Vec<Entry<'_>> {
        let mut out = Vec::new();
        for card in &cards {
            out.push(Entry::Card(card.clone()));
            out.extend(
                self.subagents
                    .under(&card.call_id)
                    .into_iter()
                    .map(Entry::Subagent),
            );
        }
        out.extend(
            self.subagents
                .rows()
                .iter()
                .filter(|row| {
                    !row.parent_tool_call_id
                        .as_ref()
                        .is_some_and(|parent| cards.iter().any(|c| &c.call_id == parent))
                })
                .map(Entry::Subagent),
        );
        out
    }

    pub fn dialog(&self, id: &str) -> Option<&OpenDialog> {
        self.dialogs.get(id)
    }

    pub fn focus_composer(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.input.focus_handle(cx), cx);
    }

    /// The composer's text.
    pub fn prompt_text(&self, cx: &App) -> String {
        self.input.read(cx).text(cx)
    }

    /// Images attached to the next prompt.
    pub fn attached_images(&self) -> &[ImageContent] {
        &self.images
    }

    /// OMP's process id while it runs.
    pub fn omp_pid(&self) -> Option<u32> {
        self.link.as_ref().and_then(OmpLink::pid)
    }

    /// Park OMP's thread until the sender is dropped.
    #[cfg(any(test, feature = "test-support"))]
    pub fn hold_omp(&self) -> std::sync::mpsc::Sender<()> {
        self.link.as_ref().expect("OMP runs").hold().unwrap()
    }

    /// Whether the link holds the current prompt in its queue, not yet sent.
    #[cfg(any(test, feature = "test-support"))]
    pub fn prompt_queued(&self) -> bool {
        self.link.as_ref().is_some_and(OmpLink::prompt_queued)
    }

    /// Whether a prompt the panel sent is still in the link or in OMP.
    #[cfg(any(test, feature = "test-support"))]
    pub fn prompt_open(&self) -> bool {
        self.link.as_ref().is_some_and(OmpLink::prompt_open)
    }

    /// Whether OMP runs a run no prompt of the panel's started.
    #[cfg(any(test, feature = "test-support"))]
    pub fn omp_runs_unprompted(&self) -> bool {
        self.link.as_ref().is_some_and(OmpLink::runs_unprompted)
    }

    /// Make the link's audit rows fail from now on.
    #[cfg(any(test, feature = "test-support"))]
    pub fn break_audit(&self) {
        if let Some(link) = &self.link {
            link.break_audit();
        }
    }

    /// The task's review: every agent transaction imported so far.
    pub fn review(&self) -> &TaskReview {
        &self.review
    }

    /// Show or hide the Review Changes view.
    pub fn toggle_review(&mut self, cx: &mut Context<Self>) {
        self.show_review = !self.show_review;
        self.review.rebuild(cx);
        self.after_review_change(cx);
    }

    /// The Accept button of one hunk: the one the person saw, by identity;
    /// a hunk that moved since is refused with a notice.
    pub fn accept_hunk(&mut self, path: &std::path::Path, key: &HunkKey, cx: &mut Context<Self>) {
        let result = self.review.accept(path, key, cx);
        self.report_review(result, cx);
    }

    /// The Reject button of one hunk: the baseline lines come back as one
    /// transaction (one native undo restores the agent's text).
    pub fn reject_hunk(&mut self, path: &std::path::Path, key: &HunkKey, cx: &mut Context<Self>) {
        let result = self.review.reject(path, key, cx).map(|_| ());
        self.report_review(result, cx);
    }

    /// Rebuild the review on every edit to a reviewed buffer.
    fn watch_reviewed_buffers(&mut self, cx: &mut Context<Self>) {
        let buffers: Vec<Entity<Buffer>> = self
            .review
            .files()
            .iter()
            .map(|f| f.buffer().clone())
            .filter(|b| !self.watched.contains(&b.entity_id()))
            .collect();
        for buffer in buffers {
            self.watched.insert(buffer.entity_id());
            self.buffer_subscriptions
                .push(cx.subscribe(&buffer, |this, _, event, cx| {
                    if matches!(event, BufferEvent::Edited { .. }) {
                        this.review.rebuild(cx);
                        this.after_review_change(cx);
                    }
                }));
        }
    }

    /// The Accept all button. With STALE hunks in the set it asks first
    /// whether to include them (owner ruling 2026-10-08); the answer comes
    /// through [`Self::accept_all_answered`].
    pub fn accept_all(&mut self, cx: &mut Context<Self>) {
        if self.review.stale_count() > 0 {
            self.confirm_accept_all = true;
            cx.notify();
            return;
        }
        self.accept_all_answered(false, cx);
    }

    /// Yes: the STALE hunks too. No: only the others.
    pub fn accept_all_answered(&mut self, include_stale: bool, cx: &mut Context<Self>) {
        self.confirm_accept_all = false;
        let result = self.review.accept_all(include_stale, cx).map(|accepted| {
            self.notice = Some(format!("accepted {} hunk(s)", accepted.len()));
        });
        self.report_review(result, cx);
    }

    /// The Revert turn button: reject every pending hunk the turn made.
    pub fn revert_turn(&mut self, turn: u32, cx: &mut Context<Self>) {
        let result = self.review.revert_turn(turn, cx).map(|event| {
            self.notice = Some(event.to_string());
        });
        self.report_review(result, cx);
    }

    fn report_review(&mut self, result: Result<(), ReviewError>, cx: &mut Context<Self>) {
        if let Err(e) = result {
            self.notice = Some(e.to_string());
        }
        self.after_review_change(cx);
    }

    fn after_review_change(&mut self, cx: &mut Context<Self>) {
        for event in self.review.drain_events() {
            let (kind, row) = event.correction();
            let task = self.review.task_id().to_string();
            let recorded = self
                .state_dir(cx)
                .and_then(|dir| cedian_shell::corrections::record(&dir, &task, kind, row));
            if let Err(e) = recorded {
                self.ledger_error =
                    Some(format!("the correction ledger did not record {event}: {e}"));
            }
        }
        if self.review.stale_count() == 0 {
            self.confirm_accept_all = false;
        }
        cx.notify();
    }

    /// Where this workspace's ledgers live (ADR-0044).
    fn state_dir(&mut self, cx: &App) -> Result<PathBuf, String> {
        if let Some(dir) = &self.state_dir {
            return Ok(dir.clone());
        }
        let root = self.workspace_root(cx).ok_or("no folder is open")?;
        let dir = cedian_shell::state::dir(&root)?;
        self.state_dir = Some(dir.clone());
        Ok(dir)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_state_dir(&mut self, dir: PathBuf) {
        self.state_dir = Some(dir);
    }

    fn workspace_root(&self, cx: &App) -> Option<PathBuf> {
        let worktree = self.project.read(cx).visible_worktrees(cx).next()?;
        Some(worktree.read(cx).abs_path().to_path_buf())
    }

    /// Show or hide the OMP settings page; it is built on first open.
    pub fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_settings = !self.show_settings;
        if self.show_settings && self.settings.is_none() {
            if let Some(root) = self.workspace_root(cx) {
                let project = self.project.clone();
                self.settings = Some(cx.new(|cx| OmpSettings::new(project, root, window, cx)));
            }
        }
        cx.notify();
    }

    pub fn omp_settings(&self) -> Option<&Entity<OmpSettings>> {
        self.settings.as_ref()
    }

    /// Start (or restart) OMP for the open folder.
    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let previous = self.link.take();
        self.forget_old_omp(window, cx);
        self.dialogs.clear();
        self.release_in_flight(cx);
        self.set_turn(Turn::Idle);
        self.notice = None;
        let Some(root) = self.workspace_root(cx) else {
            self.connection = Connection::Stopped("open a folder first".to_string());
            return;
        };
        let mut spec = match LaunchSpec::resolve(&root) {
            Ok(spec) => spec,
            Err(e) => {
                self.connection = Connection::Stopped(e);
                cx.notify();
                return;
            }
        };
        self.state_dir = Some(spec.state_dir.clone());
        self.refresh_workflow(cx);
        if self.browser.is_none() {
            self.open_browser_host(&spec.state_dir, cx);
        }
        spec.policy.browser_cdp_url = self.browser.as_ref().map(|b| b.url());
        let (read_tx, read_rx) = mpsc::unbounded::<context::Read>();
        spec.uris.push(context::scheme(read_tx));
        let this = cx.entity().downgrade();
        self._context = Some(context::serve(
            read_rx,
            move |url, cx| {
                this.update(cx, |this, cx| {
                    context::answer(url, this.workspace.as_ref(), &this.project, cx)
                })
                .ok()
            },
            cx,
        ));
        let (event_tx, mut event_rx) = mpsc::unbounded::<LinkEvent>();
        self.link = Some(OmpLink::start(spec, event_tx, previous));
        self.connection = Connection::Starting;
        self._events = Some(cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = event_rx.next().await {
                if this
                    .update_in(cx, |this, window, cx| this.on_link_event(event, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// Listen on the workspace's browser endpoint; Chromium starts on the
    /// first connection.
    fn open_browser_host(&mut self, state_dir: &Path, cx: &mut Context<Self>) {
        let (tx, mut rx) = mpsc::unbounded::<()>();
        match BrowserHost::open(
            state_dir.join("browser-profile"),
            crate::browser::executable(),
            move || {
                let _ = tx.unbounded_send(());
            },
        ) {
            Ok(host) => {
                self.browser = Some(Arc::new(host));
                self._browser_quit = Some(cx.on_app_quit(|this: &mut Self, _| {
                    if let Some(host) = &this.browser {
                        host.close_now(BROWSER_QUIT_LIMIT);
                    }
                    async {}
                }));
            }
            Err(e) => {
                self.notice = Some(e);
                return;
            }
        }
        self._browser_events = Some(cx.spawn(async move |this, cx| {
            while rx.next().await.is_some() {
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        }));
    }

    pub fn browser(&self) -> Option<&Arc<BrowserHost>> {
        self.browser.as_ref()
    }

    /// The "Open browser" button: start Chromium now.
    pub fn open_browser(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.browser.clone() else {
            return;
        };
        cx.background_spawn(async move {
            let _ = host.start();
        })
        .detach();
    }

    /// The browser holds OMP's traffic on the person's input only while a
    /// turn runs.
    fn set_turn(&mut self, turn: Turn) {
        let running = matches!(turn, Turn::Queued | Turn::Streaming | Turn::Stopping);
        self.turn = turn;
        if let Some(host) = &self.browser {
            host.set_turn(running);
        }
    }

    /// The person lets the agent use the browser again.
    pub fn resume_browser(&mut self, cx: &mut Context<Self>) {
        if let Some(host) = &self.browser {
            host.resume();
        }
        cx.notify();
    }

    /// The state gates are judged against: the shared browser's current
    /// frame sequence, so a capture from an earlier frame reads `stale-frame`.
    pub fn current_state(&self) -> CurrentState {
        let mut state = CurrentState::default();
        state.frame_seq = self
            .browser
            .as_ref()
            .map(|b| b.state())
            .filter(|s| s.running)
            .map(|s| s.seq);
        state
    }

    /// `gate` judged on the panel's browser evidence, now.
    pub fn evaluate_gate(&self, gate: &Gate) -> GateResult {
        gate.evaluate(&self.browser_evidence, &self.current_state())
    }

    /// Capture the shared page: screenshot, DOM, console and network.
    pub fn capture_browser(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.browser.clone() else {
            return;
        };
        let task = cx.background_spawn(async move { host.capture() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(capture) => {
                        let code = this.current_state().bind(&[]);
                        this.browser_evidence.push(
                            capture
                                .evidence(
                                    &format!("browser-frame-{}", capture.seq),
                                    &[BROWSER_GATE],
                                    Outcome::Pass,
                                )
                                .with_code_state(code),
                        );
                    }
                    Err(e) => this.notice = Some(format!("browser capture failed: {e}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The shared browser: its latest capture, with the frame it was taken
    /// at, and the console and network lines it carries.
    fn render_browser(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.browser.as_ref()?.state();
        if !state.running && state.latest.is_none() && state.error.is_none() {
            return None;
        }
        const LINES: usize = 20;
        let tail = |lines: &[String]| -> Vec<AnyElement> {
            lines[lines.len().saturating_sub(LINES)..]
                .iter()
                .map(|l| {
                    Label::new(l.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .into_any_element()
                })
                .collect()
        };
        let capture = state.latest.as_ref().map(|c| {
            v_flex()
                .debug_selector(|| "cedian-browser-capture".to_string())
                .gap_1()
                .child(
                    gpui::img(Arc::new(gpui::Image::from_bytes(
                        gpui::ImageFormat::Png,
                        c.png.to_vec(),
                    )))
                    .w_full()
                    .h(px(180.))
                    .object_fit(gpui::ObjectFit::Contain),
                )
                .child(
                    Label::new(format!("capture at frame {} · {}", c.seq, c.url))
                        .size(LabelSize::Small),
                )
                .child(Label::new("Console").size(LabelSize::Small))
                .children(tail(&c.console))
                .child(Label::new("Network").size(LabelSize::Small))
                .children(tail(&c.network))
        });
        Some(
            v_flex()
                .debug_selector(|| "cedian-browser".to_string())
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new(if state.running {
                                format!("Browser · frame {} · {}", state.seq, state.url)
                            } else {
                                "Browser closed".to_string()
                            })
                            .size(LabelSize::Small),
                        )
                        .when(state.running, |row| {
                            row.child(
                                Button::new("cedian-browser-capture", "Capture").on_click(
                                    cx.listener(|this, _, _, cx| this.capture_browser(cx)),
                                ),
                            )
                        }),
                )
                .when(state.preempted, |col| {
                    col.child(
                        h_flex()
                            .debug_selector(|| "cedian-browser-held".to_string())
                            .gap_2()
                            .child(
                                Label::new(
                                    "You are using the browser: the agent's next browser call waits for you",
                                )
                                .size(LabelSize::Small)
                                .color(Color::Warning),
                            )
                            .child(
                                Button::new("cedian-browser-resume", "Let the agent continue")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.resume_browser(cx)),
                                    ),
                            ),
                    )
                })
                .when_some(state.error, |col, e| {
                    col.child(Label::new(e).size(LabelSize::Small).color(Color::Error))
                })
                .children(capture)
                .into_any_element(),
        )
    }

    /// The prompt as OMP gets it: the bounded snapshot of what the person
    /// sees (§39), then their text.
    fn with_context(&self, text: &str, cx: &App) -> String {
        let snapshot = context::host(self.workspace.as_ref(), &self.project, cx)
            .map(|host| render_snapshot(&capture_ambient(&host)))
            .unwrap_or_default();
        if snapshot.is_empty() {
            text.to_string()
        } else {
            format!("{snapshot}\n{text}")
        }
    }

    /// The Retry button of a taken session.
    pub fn retry_session(&mut self, cx: &mut Context<Self>) {
        match self.link.as_ref().map(OmpLink::retry) {
            Some(Ok(())) => self.connection = Connection::Checking,
            Some(Err(e)) => self.notice = Some(e),
            None => {}
        }
        cx.notify();
    }

    /// The "Start a new session" button of a taken session.
    pub fn start_new_session(&mut self, cx: &mut Context<Self>) {
        if let Some(Err(e)) = self.link.as_ref().map(OmpLink::new_session) {
            self.notice = Some(e);
        }
        cx.notify();
    }

    /// The old OMP's subagents and queue die with it: their rows go, and
    /// what was queued goes back to the composer.
    fn forget_old_omp(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.forget_subagents();
        let queued = self.queued();
        if !queued.is_empty() {
            self.thread.apply(&RouterEvent::Queue {
                steering: Vec::new(),
                follow_up: Vec::new(),
            });
            self.restore_to_composer(queued, window, cx);
        }
    }

    fn forget_subagents(&mut self) {
        self.subagents = SubagentTree::default();
        self.steer_boxes.clear();
        self.subagent_notes.clear();
    }

    /// What OMP last said it has queued, oldest steer first.
    fn queued(&self) -> Vec<String> {
        self.thread
            .events()
            .iter()
            .rev()
            .find_map(|event| match event {
                ThreadEvent::Queue {
                    steering,
                    follow_up,
                } => Some(steering.iter().chain(follow_up).cloned().collect()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Put `texts` back in the composer, ahead of what is typed there now.
    fn restore_to_composer(
        &mut self,
        mut texts: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let typed = self.input.read(cx).text(cx);
        if !typed.is_empty() {
            texts.push(typed);
        }
        let text = texts.join("\n");
        self.input
            .update(cx, |editor, cx| editor.set_text(text, window, cx));
    }

    /// The Restart button: a fresh OMP on the same session.
    pub fn restart(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.start(window, cx);
    }

    fn on_link_event(&mut self, event: LinkEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            LinkEvent::Ready {
                session_id,
                resumed,
                policy_note,
                ..
            } => {
                self.forget_subagents();
                self.connection = Connection::Ready {
                    session_id,
                    resumed,
                    policy_note,
                };
            }
            LinkEvent::Failed(reason) => self.stop(reason, cx),
            LinkEvent::Taken { session_id, reason } => {
                if self.turn != Turn::Idle {
                    self.thread.withdraw_user();
                    self.end_turn(None, cx);
                }
                self.connection = Connection::Taken { session_id, reason };
            }
            LinkEvent::PromptCancelled => {
                self.thread.withdraw_user();
                self.end_turn(None, cx);
            }
            LinkEvent::PromptStopped(e) => {
                self.end_turn(Some(format!("turn stopped: {e}")), cx);
            }
            LinkEvent::PromptFailed(e) => {
                self.end_turn(Some(format!("OMP did not run the prompt: {e}")), cx);
            }
            LinkEvent::AbortFailed(e) => {
                if self.turn != Turn::Idle {
                    self.set_turn(Turn::Failed(format!("stop: {e}")));
                }
            }
            LinkEvent::AuditFailed(e) => self.audit_failed(e, cx),
            LinkEvent::Queued(text) => {
                if self.input.read(cx).text(cx) == text {
                    self.input.update(cx, |editor, cx| editor.clear(window, cx));
                }
            }
            LinkEvent::QueueRefused(e) => self.notice = Some(format!("OMP did not queue it: {e}")),
            LinkEvent::Restored(texts) => self.restore_to_composer(texts, window, cx),
            LinkEvent::TakeBackNotice(notice) => self.notice = Some(notice),
            LinkEvent::SubagentSteered { id, result } => {
                let note = match result {
                    Ok(()) => "steered".to_string(),
                    Err(e) => format!("steer refused: {e}"),
                };
                self.subagent_notes.insert(id, note);
            }
            LinkEvent::SubagentCancelled { id, result } => {
                let note = match result {
                    Ok(true) => "cancelled".to_string(),
                    Ok(false) => "already ended".to_string(),
                    Err(e) => format!("cancel failed: {e}"),
                };
                self.subagent_notes.insert(id, note);
            }
            LinkEvent::Event(RouterEvent::UiRequest(ExtensionUiRequest::Cancel(cancel))) => {
                self.dialogs.shift_remove(&cancel.target_id);
            }
            LinkEvent::Event(RouterEvent::UiRequest(request)) => {
                let id = cedian_omp::dialog::dialog(&request).map(|(id, _)| id.to_string());
                if let Some(id) = id
                    && self.link.as_ref().is_some_and(|link| link.is_open(&id))
                    && let Some(mut dialog) = OpenDialog::new(request, window, cx)
                {
                    let expiring = id.clone();
                    dialog.expiry = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(DIALOG_TIMEOUT).await;
                        this.update(cx, |this, cx| this.expire(&expiring, cx)).ok();
                    }));
                    self.dialogs.insert(id, dialog);
                }
            }
            LinkEvent::Event(RouterEvent::Disconnected) => {
                self.thread.apply(&RouterEvent::Disconnected);
                self.stop("OMP stopped: its process exited".to_string(), cx);
            }
            LinkEvent::Event(RouterEvent::Unknown { frame_type })
                if frame_type == "config_update" =>
            {
                if let Some(settings) = &self.settings {
                    settings.update(cx, |settings, cx| settings.reload(cx));
                }
            }
            LinkEvent::Event(event) => return self.on_event(event, cx),
        }
        cx.notify();
    }

    /// The prompt call ended without OMP settling: idle again, a call whose
    /// ToolEnd never came is released, and a failed turn keeps its reason
    /// over `notice`.
    fn end_turn(&mut self, notice: Option<String>, cx: &mut Context<Self>) {
        self.review.end_turn();
        self.release_in_flight(cx);
        if let Turn::Failed(reason) = std::mem::replace(&mut self.turn, Turn::Idle) {
            self.notice = Some(format!("turn failed: {reason}"));
        } else if notice.is_some() {
            self.notice = notice;
        }
    }

    /// Release every call still waiting for its ToolEnd: it is not coming.
    fn release_in_flight(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<String> = self
            .calls
            .iter()
            .filter(|(_, c)| c.ended.is_none())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let call = self.calls.remove(&id).expect("listed above");
            self.release(&id, call, cx);
        }
    }

    fn stop(&mut self, reason: String, cx: &mut Context<Self>) {
        self.link = None;
        self.dialogs.clear();
        self.subagents.apply(&RouterEvent::Disconnected);
        for (id, call) in std::mem::take(&mut self.calls) {
            self.release(&id, call, cx);
        }
        self.review.end_turn();
        self.end_workflow_turn(cx);
        self.set_turn(Turn::Idle);
        self.connection = Connection::Stopped(reason);
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text(cx);
        if text.trim().is_empty() {
            return;
        }
        match self.turn {
            Turn::Idle => {}
            Turn::Streaming => return self.queue(false, cx),
            _ => {
                self.notice = Some("the turn is starting or stopping; wait for it".to_string());
                cx.notify();
                return;
            }
        }
        if self.connection == Connection::NotStarted {
            self.start(window, cx);
        }
        let sent = match (&self.connection, &self.link) {
            (Connection::Stopped(reason), _) => Err(format!("{reason}; restart OMP")),
            (Connection::Taken { reason, .. }, _) => {
                Err(format!("{reason}; start a new session or retry"))
            }
            (_, Some(link)) => link.send(Prompt {
                text: self.with_context(&text, cx),
                images: self.images.clone(),
            }),
            (_, None) => Err("OMP is not running; restart it".to_string()),
        };
        if let Err(e) = sent {
            self.notice = Some(e);
            cx.notify();
            return;
        }
        self.input.update(cx, |editor, cx| editor.clear(window, cx));
        self.images.clear();
        self.thread.push_user(&text);
        self.review.begin_turn();
        self.notice = None;
        self.set_turn(Turn::Queued);
        cx.notify();
    }

    /// The Steer button: `text` goes into the running turn.
    pub fn steer(&mut self, cx: &mut Context<Self>) {
        if self.turn == Turn::Streaming {
            self.queue(true, cx);
        }
    }

    /// Steer (`steer`) or queue after the turn (`follow_up`, ADR-0050
    /// decision 3). The chip comes from OMP's own `queue_update`.
    fn queue(&mut self, steer: bool, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text(cx);
        if text.trim().is_empty() {
            return;
        }
        let Some(link) = &self.link else {
            self.notice = Some("OMP is not running; restart it".to_string());
            cx.notify();
            return;
        };
        link.queue(text, steer);
        self.notice = None;
        cx.notify();
    }

    fn on_event(&mut self, event: RouterEvent, cx: &mut Context<Self>) {
        match &event {
            RouterEvent::ToolStart {
                tool_call_id,
                tool_name,
                paths,
                ..
            } if EDIT_TOOLS.contains(&tool_name.as_str()) => {
                self.mark_before_write(tool_call_id.clone(), paths, cx);
            }
            RouterEvent::ToolStart {
                tool_call_id,
                tool_name,
                ..
            } if tool_name == BASH_TOOL => self.mark_for_bash(tool_call_id.clone(), cx),
            RouterEvent::ToolEnd {
                tool_call_id,
                is_error,
                before,
                ..
            } => {
                let root = self.workspace_root(cx);
                if let Some(call) = self.calls.get_mut(tool_call_id) {
                    call.ended = Some(*is_error);
                    call.before = before
                        .iter()
                        .filter_map(|(path, text)| {
                            let path = Path::new(path);
                            let path = if path.is_absolute() {
                                path.to_path_buf()
                            } else {
                                root.as_ref()?.join(path)
                            };
                            Some((path, text.clone()))
                        })
                        .collect();
                    self.import_when_ready(tool_call_id.clone(), cx);
                }
            }
            // OMP started a run of its own (a queued steer it drains): it
            // shows as running so Stop can stop it, also while an earlier
            // Stop still waits for OMP to settle.
            RouterEvent::AgentStart
                if matches!(self.turn, Turn::Queued | Turn::Idle)
                    || (self.turn == Turn::Stopping
                        && self.link.as_ref().is_some_and(OmpLink::runs_own)) =>
            {
                self.set_turn(Turn::Streaming)
            }
            RouterEvent::Settled => {
                self.review.end_turn();
                if let Turn::Failed(reason) = std::mem::replace(&mut self.turn, Turn::Idle) {
                    self.notice = Some(format!("turn failed: {reason}"));
                }
                self.end_workflow_turn(cx);
                self.refresh_workflow(cx);
            }
            _ => {}
        }
        self.subagents.apply(&event);
        self.thread.apply(&event);
        cx.notify();
    }

    /// Turn boundary (ADR-0036): a claim refused in the turn that just
    /// ended blocks the workflow, and the panel says which gates are unmet.
    fn end_workflow_turn(&mut self, cx: &mut Context<Self>) {
        if self.state_dir.is_none() && self.workspace_root(cx).is_none() {
            return;
        }
        let turn = Some(self.review.current_turn());
        let said = match self
            .state_dir(cx)
            .and_then(|dir| cedian_shell::workflow_host::end_turn(&dir, TASK_ID, turn))
        {
            Ok(None) => return,
            Ok(Some((status, missing))) => format!(
                "workflow {}: the agent claimed done with required gates unmet: {}",
                if status == cedian_workflow::WorkflowStatus::Failed {
                    "failed"
                } else {
                    "blocked"
                },
                missing.join("; ")
            ),
            Err(e) => format!("the workflow could not end its turn: {e}"),
        };
        self.notice = Some(match self.notice.take() {
            Some(notice) => format!("{notice}; {said}"),
            None => said,
        });
    }

    /// The task's workflow as the panel shows it.
    pub fn workflow(&self) -> Option<&WorkflowView> {
        self.workflow.as_ref()
    }

    /// Read the workflow again, off the UI thread: its gates are evaluated
    /// against the workspace hashed now (row H).
    fn refresh_workflow(&mut self, cx: &mut Context<Self>) {
        let (Ok(dir), Some(root)) = (self.state_dir(cx), self.workspace_root(cx)) else {
            return;
        };
        let live = |status| matches!(status, WorkflowStatus::Running | WorkflowStatus::Blocked);
        // A finished workflow's gates no longer change: once shown, it is
        // not hashed again on every turn. `None` keeps the view.
        let shown_finished = self.workflow.as_ref().is_some_and(|v| !live(v.status));
        let read = cx.background_spawn(async move {
            if !cedian_shell::workflow_store::exists(&dir) {
                return Ok(Some(None));
            }
            let state = cedian_shell::workflow_store::load(&dir)?;
            if shown_finished && !live(state.status) {
                return Ok(None);
            }
            let current = cedian_shell::workflow_host::current_state(&root);
            Ok::<_, String>(Some(Some(WorkflowView::new(&state, &current))))
        });
        self._workflow_load = Some(cx.spawn(async move |this, cx| {
            let view = read.await;
            this.update(cx, |this, cx| {
                match view {
                    Ok(Some(view)) => this.workflow = view,
                    Ok(None) => {}
                    Err(e) => this.notice = Some(format!("the workflow could not be read: {e}")),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// §54: during a workflow, a question nobody answered blocks its
    /// current phase and waits for the person's Resume.
    fn escalate(&mut self, why: &str, cx: &mut Context<Self>) {
        if self.state_dir.is_none() && self.workspace_root(cx).is_none() {
            return;
        }
        let turn = Some(self.review.current_turn());
        match self
            .state_dir(cx)
            .and_then(|dir| cedian_shell::workflow_host::escalate(&dir, TASK_ID, turn, why))
        {
            Ok(true) => {
                self.escalation = Some(format!(
                    "{why}: the workflow is blocked at its current phase until you resume it"
                ));
                self.refresh_workflow(cx);
            }
            Ok(false) => {}
            Err(e) => self.escalation = Some(format!("{why}; the workflow could not block: {e}")),
        }
    }

    /// The Resume button: the person's answer to a blocked workflow (§54).
    pub fn resume_workflow(&mut self, cx: &mut Context<Self>) {
        let resumed = self
            .state_dir(cx)
            .and_then(|dir| cedian_shell::workflow_host::resume(&dir));
        match resumed {
            Ok(()) => self.escalation = None,
            Err(e) => self.notice = Some(format!("the workflow did not resume: {e}")),
        }
        self.refresh_workflow(cx);
        cx.notify();
    }

    fn render_workflow(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let view = self.workflow.as_ref()?;
        let line = |text: String, color: Color| {
            Label::new(text)
                .size(LabelSize::Small)
                .color(color)
                .into_any_element()
        };
        let phases = view
            .phases
            .iter()
            .map(|(id, mark)| format!("{mark} {id}"))
            .collect::<Vec<_>>()
            .join("  ");
        let gates = view.gates.iter().map(|g| {
            let id = g.id.clone();
            div()
                .debug_selector(move || format!("cedian-workflow-gate-{id}"))
                .child(line(
                    format!(
                        "{}{} {:?}: {}",
                        g.id,
                        if g.required { " (required)" } else { "" },
                        g.status,
                        g.reason
                    ),
                    if g.status == cedian_workflow::GateStatus::Passed {
                        Color::Default
                    } else {
                        Color::Warning
                    },
                ))
                .into_any_element()
        });
        let blocked = view.status == cedian_workflow::WorkflowStatus::Blocked;
        Some(
            v_flex()
                .debug_selector(|| "cedian-workflow".to_string())
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new(format!("Workflow · {} · {:?}", view.title, view.status))
                                .size(LabelSize::Small),
                        )
                        .when(blocked, |row| {
                            row.child(
                                div()
                                    .debug_selector(|| "cedian-workflow-resume".to_string())
                                    .child(
                                        Button::new("cedian-workflow-resume", "Resume").on_click(
                                            cx.listener(|this, _, _, cx| this.resume_workflow(cx)),
                                        ),
                                    ),
                            )
                        }),
                )
                .when_some(self.escalation.clone().filter(|_| blocked), |col, why| {
                    col.child(
                        div()
                            .debug_selector(|| "cedian-workflow-escalation".to_string())
                            .child(line(why, Color::Warning)),
                    )
                })
                .child(line(phases, Color::Muted))
                .children(gates)
                .children(view.evidence.iter().map(|e| line(e.clone(), Color::Muted)))
                .children(
                    view.claims
                        .iter()
                        .flat_map(|c| c.lines())
                        .map(|l| line(l.to_string(), Color::Muted)),
                )
                .into_any_element(),
        )
    }

    /// The Stop button: close the dialogs OMP waits on and abort the
    /// running turn. The turn goes idle when OMP reports the session settled.
    pub fn stop_turn(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.turn, Turn::Queued | Turn::Streaming) {
            return;
        }
        self.set_turn(Turn::Stopping);
        let closed = self.link.as_ref().map(OmpLink::close_dialogs);
        self.dialogs.clear();
        match closed {
            Some(Err(e)) => self.audit_failed(e, cx),
            _ => self.cancel(),
        }
        cx.notify();
    }

    /// ADR-0035: an unaudited turn fails. The first failure aborts it; later
    /// ones in the same turn change nothing.
    fn audit_failed(&mut self, e: String, cx: &mut Context<Self>) {
        let reason = format!("audit: {e}");
        match self.turn {
            Turn::Failed(_) => {}
            Turn::Idle => self.notice = Some(reason),
            Turn::Queued | Turn::Streaming | Turn::Stopping => {
                self.set_turn(Turn::Failed(reason));
                self.cancel();
            }
        }
        cx.notify();
    }

    fn cancel(&self) {
        if let Some(link) = &self.link {
            link.cancel();
        }
    }

    /// Answer dialog `id` as its buttons do. An answer OMP did not get
    /// leaves the dialog open with the reason on it.
    pub fn answer(&mut self, id: &str, answer: UserAnswer, cx: &mut Context<Self>) {
        let sent = match &self.link {
            Some(link) => link.answer(id, answer),
            None => Err(AnswerError::NotSent("OMP is not running".to_string())),
        };
        match sent {
            Ok(()) => {
                self.dialogs.shift_remove(id);
            }
            Err(AnswerError::NotSent(e)) => {
                if let Some(dialog) = self.dialogs.get_mut(id) {
                    dialog.error = Some(e);
                }
            }
            Err(AnswerError::Unaudited(e)) => {
                self.dialogs.shift_remove(id);
                self.audit_failed(e, cx);
            }
        }
        cx.notify();
    }

    /// The dialog's lease ran out: OMP is told it timed out, and the panel
    /// says so where the dialog was.
    fn expire(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(dialog) = self.dialogs.shift_remove(id) else {
            return;
        };
        let closed = match &self.link {
            Some(link) => link.expire(id),
            None => Ok(false),
        };
        match closed {
            Ok(true) => {
                let why = format!(
                    "\"{}\" got no answer in {} minutes; cedian dismissed it",
                    dialog.title().lines().next().unwrap_or_default(),
                    DIALOG_TIMEOUT.as_secs() / 60
                );
                self.escalate(&why, cx);
                self.notice = Some(why);
            }
            Ok(false) => {}
            Err(e) => self.audit_failed(e, cx),
        }
        cx.notify();
    }

    fn submit_dialog(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(dialog) = self.dialogs.get_mut(id) else {
            return;
        };
        match dialog.submission(cx) {
            Ok(answer) => self.answer(id, answer, cx),
            Err(e) => {
                dialog.error = Some(e);
                cx.notify();
            }
        }
    }

    /// Paste into the composer: clipboard images attach to the next
    /// prompt and any text goes on to the editor. Elsewhere in the panel
    /// (a dialog's text box) paste is the editor's alone.
    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        cx.propagate();
        if !self.input.focus_handle(cx).is_focused(window) {
            return;
        }
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let has_text = item.text().is_some();
        let images: Vec<_> = item
            .into_entries()
            .filter_map(|entry| match entry {
                ClipboardEntry::Image(image) => Some(cedian_omp::image_content(
                    image.format.mime_type(),
                    &image.bytes,
                )),
                _ => None,
            })
            .collect();
        if images.is_empty() {
            return;
        }
        if !has_text {
            cx.stop_propagation();
        }
        self.images.extend(images);
        cx.notify();
    }

    fn render_subagent(&self, row: &SubagentRow, cx: &mut Context<Self>) -> AnyElement {
        let id = row.id.clone();
        let selector = format!("cedian-subagent-{id}");
        type OnClick = fn(&mut CedianPanel, &str, &mut Window, &mut Context<CedianPanel>);
        let button = |label: &'static str, suffix: &str, on: OnClick, cx: &mut Context<Self>| {
            let selector = format!("cedian-subagent-{id}-{suffix}");
            let element = ElementId::Name(selector.clone().into());
            let id = id.clone();
            div().debug_selector(move || selector).child(
                Button::new(element, label)
                    .on_click(cx.listener(move |this, _, window, cx| on(this, &id, window, cx))),
            )
        };
        let controls = (row.status == SubagentStatus::Running)
            .then(|| self.steer_boxes.get(&row.id).cloned())
            .flatten()
            .map(|input| {
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(input))
                    .child(button("Steer", "steer", Self::steer_subagent, cx))
                    .child(button(
                        "Cancel",
                        "cancel",
                        |this, id, _, cx| this.cancel_subagent(id, cx),
                        cx,
                    ))
            });
        v_flex()
            .debug_selector(move || selector)
            .pl_4()
            .child(Label::new(subagent_line(row)).size(LabelSize::Small))
            .children(controls)
            .children(self.subagent_notes.get(&row.id).map(|note| {
                Label::new(note.clone())
                    .size(LabelSize::Small)
                    .color(Color::Warning)
            }))
            .into_any_element()
    }

    fn render_dialog(&self, id: &str, dialog: &OpenDialog, cx: &mut Context<Self>) -> AnyElement {
        let button = |label: &str, answer: Option<UserAnswer>| {
            let id = id.to_string();
            let element = ElementId::Name(format!("cedian-dialog-{id}-{label}").into());
            div()
                .debug_selector(|| format!("cedian-dialog-{id}-{label}"))
                .child(
                    Button::new(element, label.to_string()).on_click(cx.listener(
                        move |this, _, _, cx| match answer.clone() {
                            Some(answer) => this.answer(&id, answer, cx),
                            None => this.submit_dialog(&id, cx),
                        },
                    )),
                )
        };
        let mut body = v_flex()
            .debug_selector(|| format!("cedian-dialog-{id}"))
            .p_2()
            .gap_1()
            .border_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new(dialog.title().to_string()));
        let mut buttons = h_flex().gap_2().flex_wrap();
        match dialog.request() {
            ExtensionUiRequest::Select(r) => {
                for option in &r.options {
                    buttons =
                        buttons.child(button(option, Some(UserAnswer::Choice(option.clone()))));
                }
            }
            ExtensionUiRequest::Confirm(r) => {
                body = body.child(Label::new(r.message.clone()).color(Color::Muted));
                buttons = buttons
                    .child(button("Yes", Some(UserAnswer::Confirm(true))))
                    .child(button("No", Some(UserAnswer::Confirm(false))));
            }
            _ => {}
        }
        if let Some(ask) = dialog.ask() {
            for (n, question) in ask.questions().iter().enumerate() {
                let mut options = h_flex().gap_1().flex_wrap();
                for option in &question.options {
                    let (id, qid, label) = (id.to_string(), question.id.clone(), option.clone());
                    let selector = format!("cedian-ask-{id}-{qid}-{label}");
                    let element = ElementId::Name(selector.clone().into());
                    options = options.child(
                        div().debug_selector(|| selector).child(
                            Button::new(element, option.clone())
                                .toggle_state(question.selected.contains(option))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(dialog) = this.dialogs.get_mut(&id) {
                                        dialog.toggle_option(&qid, &label);
                                        dialog.error = None;
                                    }
                                    cx.notify();
                                })),
                        ),
                    );
                }
                body = body
                    .child(Label::new(question.question.clone()))
                    .child(options)
                    .when_some(dialog.custom_box(n).cloned(), |body, editor| {
                        body.child(editor)
                    });
            }
        }
        if let Some(text) = dialog.text_box() {
            body = body.child(text.clone());
        }
        if dialog.text_box().is_some() || dialog.ask().is_some() {
            buttons = buttons.child(button("Submit", None));
        }
        buttons = buttons.child(button("Dismiss", Some(UserAnswer::Dismiss)));
        body.child(buttons)
            .when_some(dialog.error.clone(), |body, error| {
                body.child(Label::new(error).color(Color::Error))
            })
            .into_any_element()
    }

    /// An edit-class tool starts: mark the open buffers of the files it
    /// names, and open the named files that are not open yet so their write
    /// is imported too. A file the call does not name is never an outcome:
    /// the person's own save of it during the call is not the agent's.
    /// ADR-0047: a `bash` call names no files, so every open buffer of the
    /// workspace is marked; the ones it changes become its hunks.
    fn mark_for_bash(&mut self, tool_call_id: String, cx: &mut Context<Self>) {
        let Some(root) = self.workspace_root(cx) else {
            return;
        };
        let open: Vec<PathBuf> = self
            .project
            .read(cx)
            .opened_buffers(cx)
            .into_iter()
            .filter_map(|b| b.read(cx).file()?.as_local().map(|f| f.abs_path(cx)))
            .filter(|path| path.starts_with(&root))
            .collect();
        self.mark_files(tool_call_id, open, true, cx);
    }

    /// Files the worktree saw change: a running `bash` call's, when not open.
    fn bash_changed(
        &mut self,
        project: &Entity<Project>,
        worktree: worktree::WorktreeId,
        changes: &worktree::UpdatedEntriesSet,
        cx: &mut Context<Self>,
    ) {
        if !self
            .calls
            .values()
            .any(|c| c.bash.is_some() && c.ended.is_none())
        {
            return;
        }
        let Some(worktree) = project.read(cx).worktree_for_id(worktree, cx) else {
            return;
        };
        let worktree = worktree.read(cx);
        // The initial scan's `Loaded` is no change; a directory or an
        // ignored path is no file to review. A removed file has no entry.
        let changed: Vec<(PathBuf, PathBuf)> = changes
            .iter()
            .filter(|(rel, _, change)| {
                *change != worktree::PathChange::Loaded
                    && worktree
                        .entry_for_path(rel)
                        .is_none_or(|e| e.is_file() && !e.is_ignored)
            })
            .map(|(rel, _, _)| (worktree.absolutize(rel), worktree.full_path(rel)))
            .collect();
        for call in self.calls.values_mut() {
            if let (Some(seen), None) = (call.bash.as_mut(), call.ended) {
                for path in &changed {
                    if !seen.contains(path) {
                        seen.push(path.clone());
                    }
                }
            }
        }
    }

    fn mark_before_write(
        &mut self,
        tool_call_id: String,
        paths: &[String],
        cx: &mut Context<Self>,
    ) {
        let root = self.workspace_root(cx);
        let named: Vec<PathBuf> = paths
            .iter()
            .filter_map(|p| {
                let path = std::path::Path::new(p);
                if path.is_absolute() {
                    Some(path.to_path_buf())
                } else {
                    root.as_ref().map(|r| r.join(path))
                }
            })
            .collect();
        self.mark_files(tool_call_id, named, false, cx);
    }

    fn mark_files(
        &mut self,
        tool_call_id: String,
        named: Vec<PathBuf>,
        bash: bool,
        cx: &mut Context<Self>,
    ) {
        let buffers: Vec<(Entity<Buffer>, PathBuf)> = self
            .project
            .read(cx)
            .opened_buffers(cx)
            .into_iter()
            .filter_map(|b| {
                let path = b.read(cx).file().and_then(|f| f.as_local())?.abs_path(cx);
                named.contains(&path).then_some((b, path))
            })
            .collect();
        let to_open: Vec<PathBuf> = named
            .iter()
            .filter(|p| !buffers.iter().any(|(_, o)| o == *p))
            .cloned()
            .collect();
        // A buffer dirty before the call differs from its disk: its disk
        // text at the start is read, so a disk the call leaves alone is no
        // outcome (a clean buffer's text is its disk text).
        let mut dirty: Vec<(usize, PathBuf)> = Vec::new();
        let marks: Vec<(Entity<Buffer>, Mark)> = buffers
            .into_iter()
            .enumerate()
            .map(|(i, (b, path))| {
                if bash {
                    self.review.observe_bash(&b, &tool_call_id, cx);
                } else {
                    self.review.observe(&b, &tool_call_id, cx);
                }
                let mark = b.update(cx, |b, cx| import::begin(b, cx));
                if b.read(cx).is_dirty() {
                    dirty.push((i, path));
                }
                (b, mark)
            })
            .collect();
        self.calls.insert(
            tool_call_id.clone(),
            CallMarks {
                marks,
                unopened: Vec::new(),
                reading: to_open.len() + dirty.len(),
                ended: None,
                before: Vec::new(),
                bash: bash.then(Vec::new),
                overlapped: Vec::new(),
            },
        );
        let fs = self.project.read(cx).fs().clone();
        for (i, path) in dirty {
            let fs = fs.clone();
            let id = tool_call_id.clone();
            cx.spawn(async move |this, cx| {
                let before = fs.load(&path).await.ok();
                this.update(cx, |this, cx| {
                    let Some(call) = this.calls.get_mut(&id) else {
                        return;
                    };
                    if let Some(mut before) = before {
                        text::LineEnding::normalize(&mut before);
                        call.marks[i].1.set_disk_at_start(before);
                    }
                    call.reading -= 1;
                    this.import_when_ready(id, cx);
                })
                .ok();
            })
            .detach();
        }
        for path in to_open {
            let fs = fs.clone();
            let id = tool_call_id.clone();
            cx.spawn(async move |this, cx| {
                let before = fs.load(&path).await.ok();
                this.update(cx, |this, cx| {
                    let Some(call) = this.calls.get_mut(&id) else {
                        return;
                    };
                    call.unopened.push((path, before));
                    call.reading -= 1;
                    this.import_when_ready(id, cx);
                })
                .ok();
            })
            .detach();
        }
    }

    /// Import a call's files once its `ToolEnd` came and every disk read
    /// finished: open the files that were not open, then import each file
    /// as one transaction.
    fn import_when_ready(&mut self, tool_call_id: String, cx: &mut Context<Self>) {
        let ready = self
            .calls
            .get(&tool_call_id)
            .is_some_and(|c| c.reading == 0 && c.ended.is_some());
        if !ready {
            return;
        }
        let mut call = self.calls.remove(&tool_call_id).expect("checked above");
        // A failed edit wrote nothing; a failed command may have written.
        if call.ended == Some(true) && call.bash.is_none() {
            return self.release(&tool_call_id, call, cx);
        }
        // ADR-0055 decision 6: a buffer two calls wrote while one was a
        // `bash` call is imported by neither.
        for (other_id, other) in &mut self.calls {
            if call.bash.is_none() && other.bash.is_none() {
                continue;
            }
            for (buffer, _) in &call.marks {
                if other.marks.iter().any(|(b, _)| b == buffer) {
                    call.overlapped.push((buffer.clone(), other_id.clone()));
                    other
                        .overlapped
                        .push((buffer.clone(), tool_call_id.clone()));
                }
            }
        }
        let (overlapped, marks): (Vec<_>, Vec<_>) = std::mem::take(&mut call.marks)
            .into_iter()
            .partition(|(b, _)| call.overlapped.iter().any(|(o, _)| o == b));
        call.marks = marks;
        if let Some(changed) = &call.bash {
            for (abs, shown) in changed {
                let open = call.marks.iter().chain(&overlapped).any(|(b, _)| {
                    b.read(cx)
                        .file()
                        .and_then(|f| f.as_local())
                        .is_some_and(|f| f.abs_path(cx) == *abs)
                });
                if !open {
                    self.review.could_not_review(
                        shown.clone(),
                        format!(
                            "changed while bash call {tool_call_id} ran (it or another process \
                             wrote it); it was not open, so cedian has no text from before the \
                             call to review it against"
                        ),
                    );
                }
            }
        }
        let fs = self.project.read(cx).fs().clone();
        for (buffer, mark) in overlapped {
            let others: Vec<&str> = call
                .overlapped
                .iter()
                .filter(|(b, _)| *b == buffer)
                .map(|(_, id)| id.as_str())
                .collect();
            let reason = format!(
                "changed while calls {tool_call_id} and {} both ran, so which call wrote what is \
                 unknown; nothing was imported",
                others.join(", ")
            );
            let file = buffer.read(cx).file().cloned();
            let fs = fs.clone();
            let id = tool_call_id.clone();
            cx.spawn(async move |this, cx| {
                let abs = cx.update(|cx| {
                    file.as_ref()
                        .and_then(|f| f.as_local())
                        .map(|f| f.abs_path(cx))
                });
                let disk = match abs {
                    Some(abs) => fs.load(&abs).await.ok(),
                    None => None,
                };
                this.update(cx, |this, cx| {
                    let mut start = mark.disk_at_start();
                    text::LineEnding::normalize(&mut start);
                    if disk.is_none_or(|mut d| {
                        text::LineEnding::normalize(&mut d);
                        d != start
                    }) {
                        let path = file.map(|f| f.full_path(cx)).unwrap_or_default();
                        this.review.could_not_review(path, reason);
                        this.review.import_unattributed(&buffer, &id, cx);
                    } else {
                        this.review.import_done(&buffer, &id, cx);
                    }
                    this.after_review_change(cx);
                })
                .ok();
            })
            .detach();
        }
        let project = self.project.clone();
        cx.spawn(async move |this, cx| {
            // Files the person opened and edited meanwhile: their buffer
            // stays theirs, so a write to it imports nothing.
            let mut held: Vec<(Entity<Buffer>, String)> = Vec::new();
            // Opened files whose before-text is cedian's own read, which may
            // have run after OMP's write: why OMP gave none.
            // Open buffers too: a dirty one's read, or a clean one Zed
            // reloaded before the mark. Each is listed if it imports
            // nothing, never silently Unchanged.
            let mut unsure: Vec<(Entity<Buffer>, String)> = Vec::new();
            let omp_before =
                |path: &Path| call.before.iter().find(|(p, _)| p == path).map(|(_, t)| t);
            let id = tool_call_id.as_str();
            let lost_read = |why: &str| {
                format!(
                    "OMP's call {id} may have written it before cedian read it, and no text \
                     from before the call is left ({why}); nothing was imported"
                )
            };
            let moved = format!(
                "OMP's call {id} moved it, and OMP's text from before the call is the move's \
                 source, so cedian has no baseline for this path; nothing was imported"
            );
            let mut marks = Vec::new();
            for (buffer, mut mark) in call.marks {
                let path = buffer.read_with(cx, |b, cx| {
                    b.file().and_then(|f| f.as_local()).map(|f| f.abs_path(cx))
                });
                let why = match path.as_deref().and_then(omp_before) {
                    Some(TextBefore::Moved) => {
                        this.update(cx, |this, cx| {
                            let path = buffer.read(cx).file().map(|f| f.full_path(cx));
                            this.review
                                .could_not_review(path.unwrap_or_default(), moved.clone());
                            this.review.import_unattributed(&buffer, id, cx);
                        })
                        .ok();
                        continue;
                    }
                    Some(TextBefore::Text(text)) => {
                        let mut text = text.clone();
                        text::LineEnding::normalize(&mut text);
                        let start = mark.start().text();
                        if mark.read_disk() {
                            mark.correct_disk_at_start(&text);
                            None
                        } else {
                            (text != start).then(|| {
                                format!(
                                    "OMP's call {id} wrote it before cedian marked it (Zed had \
                                     already reloaded it); nothing was imported"
                                )
                            })
                        }
                    }
                    _ if call.bash.is_some() => None,
                    Some(TextBefore::Pruned) => Some(lost_read("OMP dropped it past 32 KiB")),
                    _ => Some(lost_read("OMP's result reports none")),
                };
                if let Some(mut why) = why {
                    if mark.read_disk() {
                        why.push_str(
                            ". It was open with your unsaved edits, which are kept: the file \
                             on disk may have changed under them, and saving them overwrites it",
                        );
                    }
                    unsure.push((buffer.clone(), why));
                }
                marks.push((buffer, mark));
            }
            for (path, read) in call.unopened {
                let (before, why) = match omp_before(&path) {
                    Some(TextBefore::Moved) => {
                        this.update(cx, |this, cx| {
                            let shown = this.shown_path(&path, cx);
                            this.review.could_not_review(shown, moved.clone());
                        })
                        .ok();
                        continue;
                    }
                    Some(TextBefore::Text(text)) => {
                        let mut text = text.clone();
                        text::LineEnding::normalize(&mut text);
                        (Some(text), None)
                    }
                    Some(TextBefore::NewFile) => (Some(String::new()), None),
                    Some(TextBefore::Pruned) => {
                        (read, Some(lost_read("OMP dropped it past 32 KiB")))
                    }
                    None => (read, Some(lost_read("OMP's result reports none"))),
                };
                let opened = project
                    .update(cx, |p, cx| p.open_local_buffer(path.clone(), cx))
                    .await;
                match opened {
                    Ok(buffer) => {
                        if let Some(why) = why {
                            unsure.push((buffer.clone(), why));
                        }
                        let before = before.unwrap_or_default();
                        let mark = buffer.update(cx, |b, cx| {
                            import::begin_from(b, &before, cx).unwrap_or_else(|| {
                                held.push((cx.entity(), before.clone()));
                                import::begin(b, cx)
                            })
                        });
                        marks.push((buffer, mark));
                    }
                    Err(e) => {
                        this.update(cx, |this, _| {
                            this.review.could_not_review(path, e.to_string());
                        })
                        .ok();
                    }
                }
            }
            for (buffer, mark) in marks {
                let outcome = import::finish(buffer.clone(), mark.clone(), cx).await;
                this.update(cx, |this, cx| {
                    match outcome {
                        Ok(ImportOutcome::Imported(transaction)) => this.review.agent_edited(
                            &buffer,
                            &tool_call_id,
                            transaction,
                            mark.start().clone(),
                            cx,
                        ),
                        Ok(ImportOutcome::Stale) => {
                            this.review.import_refused(&buffer, &tool_call_id, cx)
                        }
                        Ok(ImportOutcome::Unchanged) => {
                            let written = held
                                .iter()
                                .find(|(b, _)| *b == buffer)
                                .is_some_and(|(_, before)| buffer.read(cx).text() != *before);
                            let path = || {
                                buffer
                                    .read(cx)
                                    .file()
                                    .map(|f| f.full_path(cx))
                                    .unwrap_or_default()
                            };
                            if written {
                                this.review.could_not_review(
                                    path(),
                                    format!(
                                        "it was open with your own edits when OMP wrote it \
                                         (call {tool_call_id}); nothing was imported"
                                    ),
                                );
                            } else if let Some((_, why)) = unsure.iter().find(|(b, _)| *b == buffer)
                            {
                                this.review.could_not_review(path(), why.clone());
                            }
                            this.review.import_done(&buffer, &tool_call_id, cx);
                        }
                        Err(e) => {
                            let path = buffer.read(cx).file().map(|f| f.full_path(cx));
                            this.review
                                .could_not_review(path.unwrap_or_default(), e.to_string());
                            this.review.import_done(&buffer, &tool_call_id, cx);
                        }
                    }
                    this.still_writing(&buffer, cx);
                    this.watch_reviewed_buffers(cx);
                    this.after_review_change(cx);
                })
                .ok();
            }
            this.update(cx, |this, cx| this.after_review_change(cx))
                .ok();
        })
        .detach();
    }

    /// `abs` as the review shows paths: under its worktree's name.
    fn shown_path(&self, abs: &Path, cx: &App) -> PathBuf {
        match self.project.read(cx).find_worktree(abs, cx) {
            Some((worktree, rel)) => worktree.read(cx).full_path(&rel),
            None => abs.to_path_buf(),
        }
    }

    /// An outcome may have just created the file's review; the calls still
    /// in flight that marked the buffer are writing it, and the review
    /// only learns of a call at its observe, so they are told again.
    fn still_writing(&mut self, buffer: &Entity<Buffer>, cx: &mut Context<Self>) {
        let ids: Vec<(String, bool)> = self
            .calls
            .iter()
            .filter(|(_, c)| c.marks.iter().any(|(b, _)| b == buffer))
            .map(|(id, c)| (id.clone(), c.bash.is_some()))
            .collect();
        for (id, bash) in ids {
            if bash {
                self.review.observe_bash(buffer, &id, cx);
            } else {
                self.review.observe(buffer, &id, cx);
            }
        }
    }

    /// A call that imports nothing: its marked buffers are the user's again.
    fn release(&mut self, tool_call_id: &str, call: CallMarks, cx: &mut Context<Self>) {
        for (buffer, _) in call.marks {
            self.review.import_done(&buffer, tool_call_id, cx);
        }
        self.after_review_change(cx);
    }

    fn render_review(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let turn = self.review.current_turn();
        let mut body = v_flex()
            .id("cedian-review")
            .debug_selector(|| "cedian-review".to_string())
            .flex_1()
            .overflow_y_scroll()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new("Review changes"))
                    .child(
                        div()
                            .debug_selector(|| "cedian-accept-all".to_string())
                            .child(
                                Button::new("cedian-accept-all", "Accept all")
                                    .on_click(cx.listener(|this, _, _, cx| this.accept_all(cx))),
                            ),
                    )
                    .when(turn > 0 && self.turn == Turn::Idle, |row| {
                        row.child(
                            div()
                                .debug_selector(|| "cedian-revert-turn".to_string())
                                .child(
                                    Button::new(
                                        "cedian-revert-turn",
                                        format!("Revert turn {turn}"),
                                    )
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            this.revert_turn(turn, cx)
                                        }),
                                    ),
                                ),
                        )
                    }),
            );
        if self.confirm_accept_all {
            let stale = self.review.stale_count();
            body = body.child(
                h_flex()
                    .gap_2()
                    .debug_selector(|| "cedian-accept-all-confirm".to_string())
                    .child(
                        Label::new(format!(
                            "{stale} STALE hunk(s) were edited after the agent. Accept them too?"
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                    )
                    .child(
                        div()
                            .debug_selector(|| "cedian-accept-all-yes".to_string())
                            .child(Button::new("cedian-accept-all-yes", "Yes").on_click(
                                cx.listener(|this, _, _, cx| this.accept_all_answered(true, cx)),
                            )),
                    )
                    .child(
                        div()
                            .debug_selector(|| "cedian-accept-all-no".to_string())
                            .child(Button::new("cedian-accept-all-no", "No").on_click(
                                cx.listener(|this, _, _, cx| this.accept_all_answered(false, cx)),
                            )),
                    ),
            );
        }
        if self.review.is_empty() {
            return body
                .child(Label::new("No agent edits yet").color(Color::Muted))
                .into_any_element();
        }
        let files: Vec<_> = self
            .review
            .files()
            .iter()
            .map(|file| {
                (
                    self.review.path(file, cx),
                    file.stale_import().map(str::to_string),
                    file.hunks().to_vec(),
                )
            })
            .collect();
        for (path, reason) in self.review.unreviewable().to_vec() {
            let selector = format!("cedian-unreviewed-{}", path.display());
            body = body.child(
                v_flex()
                    .gap_1()
                    .debug_selector(move || selector)
                    .child(Label::new(path.display().to_string()).size(LabelSize::Small))
                    .child(
                        Label::new(format!(
                            "OMP wrote this file; it could not be reviewed: {reason}"
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                    ),
            );
        }
        for (path, stale_import, hunks) in files {
            let name = path.display().to_string();
            let mut section = v_flex()
                .gap_1()
                .child(Label::new(name.clone()).size(LabelSize::Small));
            if let Some(reason) = stale_import {
                section = section.child(
                    Label::new(format!("STALE: {reason}"))
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                );
            }
            for (index, hunk) in hunks.into_iter().enumerate() {
                let selector = |action: &str| format!("cedian-{action}-{name}-{index}");
                let head = format!(
                    "lines {}-{} · {:?} · {}",
                    hunk.rows.start + 1,
                    hunk.rows.end,
                    hunk.status,
                    hunk.tool_call_ids.join(", ")
                );
                let mut card = v_flex()
                    .p_1()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .child(Label::new(head).size(LabelSize::Small))
                    .children(hunk.old_text.lines().map(|l| {
                        Label::new(format!("- {l}"))
                            .size(LabelSize::Small)
                            .color(Color::Deleted)
                            .into_any_element()
                    }))
                    .children(hunk.new_text.lines().map(|l| {
                        Label::new(format!("+ {l}"))
                            .size(LabelSize::Small)
                            .color(Color::Created)
                            .into_any_element()
                    }));
                let accept = matches!(
                    hunk.status,
                    HunkStatus::Pending | HunkStatus::Unattributed | HunkStatus::Stale
                );
                let reject = matches!(hunk.status, HunkStatus::Pending | HunkStatus::Unattributed);
                let mut buttons = h_flex().gap_1();
                if accept {
                    let (p, k, s) = (path.clone(), hunk.key.clone(), selector("accept"));
                    buttons = buttons.child(div().debug_selector(|| s.clone()).child(
                        Button::new(ElementId::Name(s.clone().into()), "Accept").on_click(
                            cx.listener(move |this, _, _, cx| this.accept_hunk(&p, &k, cx)),
                        ),
                    ));
                }
                if reject {
                    let (p, k, s) = (path.clone(), hunk.key.clone(), selector("reject"));
                    buttons = buttons.child(div().debug_selector(|| s.clone()).child(
                        Button::new(ElementId::Name(s.clone().into()), "Reject").on_click(
                            cx.listener(move |this, _, _, cx| this.reject_hunk(&p, &k, cx)),
                        ),
                    ));
                }
                card = card.child(buttons);
                section = section.child(card);
            }
            body = body.child(section);
        }
        body.into_any_element()
    }
}

impl EventEmitter<PanelEvent> for CedianPanel {}

impl Focusable for CedianPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CedianPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let running = |tree: &SubagentTree, id: &str| {
            tree.rows()
                .iter()
                .any(|row| row.id == id && row.status == SubagentStatus::Running)
        };
        self.steer_boxes
            .retain(|id, _| running(&self.subagents, id));
        for row in self.subagents.rows() {
            if row.status == SubagentStatus::Running && !self.steer_boxes.contains_key(&row.id) {
                let input = cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text("Steer this subagent…", window, cx);
                    editor
                });
                self.steer_boxes.insert(row.id.clone(), input);
            }
        }
        let (messages, cards) = cedian_agent_ui::render_thread(self.thread.events());
        let mut rows: Vec<AnyElement> = messages
            .into_iter()
            .map(|m| Label::new(format!("{:?}: {}", m.role, m.text)).into_any_element())
            .collect();
        for entry in self.cards_with_subagents(cards) {
            rows.push(match entry {
                Entry::Card(card) => {
                    let selector = format!("cedian-tool-{}", card.call_id);
                    div()
                        .debug_selector(move || selector)
                        .child(Label::new(card_line(&card)).color(Color::Muted))
                        .into_any_element()
                }
                Entry::Subagent(row) => self.render_subagent(row, cx),
            });
        }
        let connection = match &self.connection {
            Connection::NotStarted => "OMP not started".to_string(),
            Connection::Starting => "starting OMP…".to_string(),
            Connection::Checking => {
                "checking whether another process drives the session…".to_string()
            }
            Connection::Ready {
                session_id,
                resumed,
                policy_note,
            } => format!(
                "OMP ready, session {} ({}){}",
                session_id.get(..8).unwrap_or(session_id),
                if *resumed { "resumed" } else { "new" },
                policy_note
                    .as_deref()
                    .map(|n| format!(" · {n}"))
                    .unwrap_or_default()
            ),
            Connection::Stopped(reason) => reason.clone(),
            Connection::Taken { session_id, reason } => format!(
                "session {} not opened: {reason}",
                session_id.get(..8).unwrap_or(session_id)
            ),
        };
        let taken = matches!(self.connection, Connection::Taken { .. });
        let stopped = matches!(self.connection, Connection::Stopped(_));
        let dialogs: Vec<AnyElement> = self
            .dialogs
            .iter()
            .map(|(id, dialog)| self.render_dialog(id, dialog, cx))
            .collect();
        let stoppable = matches!(self.turn, Turn::Queued | Turn::Streaming);
        let idle = self.turn == Turn::Idle;
        let images = self.images.len();
        let imported: usize = self
            .review
            .files()
            .iter()
            .map(|f| f.agent_txns().len())
            .sum();
        let open: usize = self
            .review
            .files()
            .iter()
            .flat_map(|f| f.hunks())
            .filter(|h| !matches!(h.status, HunkStatus::Accepted | HunkStatus::Rejected))
            .count();
        let stale_files = self
            .review
            .files()
            .iter()
            .filter(|f| f.stale_import().is_some())
            .count();
        let footer = format!(
            "{} · {} · imported {} agent edit(s){}",
            connection,
            self.turn.label(),
            imported,
            if stale_files > 0 {
                format!(" · {stale_files} STALE file(s)")
            } else {
                String::new()
            }
        );
        let review = self.show_review.then(|| self.render_review(cx));
        let browser = self.render_browser(cx);
        let workflow = self.render_workflow(cx);
        v_flex()
            .key_context("CedianPanel")
            .track_focus(&self.focus_handle)
            .capture_action(cx.listener(Self::paste))
            .size_full()
            .p_2()
            .gap_2()
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        div()
                            .debug_selector(|| "cedian-review-toggle".to_string())
                            .child(
                                Button::new(
                                    "cedian-review-toggle",
                                    if self.show_review {
                                        "Back to chat".to_string()
                                    } else {
                                        format!("Review changes ({open})")
                                    },
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_review(cx))),
                            ),
                    )
                    .child(
                        div()
                            .debug_selector(|| "cedian-open-browser".to_string())
                            .child(
                                Button::new("cedian-open-browser", "Open browser")
                                    .disabled(self.browser.is_none())
                                    .on_click(cx.listener(|this, _, _, cx| this.open_browser(cx))),
                            ),
                    )
                    .child(
                        Button::new(
                            "cedian-settings",
                            if self.show_settings {
                                "Back to chat"
                            } else {
                                "OMP settings"
                            },
                        )
                        .on_click(
                            cx.listener(|this, _, window, cx| this.toggle_settings(window, cx)),
                        ),
                    ),
            )
            .when_some(review, |panel, review| panel.child(review))
            .when_some(
                self.settings
                    .clone()
                    .filter(|_| self.show_settings && !self.show_review),
                |panel, settings| panel.child(div().flex_1().child(settings)),
            )
            .when(!self.show_settings && !self.show_review, |panel| {
                panel.child(
                    v_flex()
                        .id("cedian-thread")
                        .flex_1()
                        .overflow_y_scroll()
                        .gap_1()
                        .children(rows),
                )
            })
            .children(workflow)
            .children(browser)
            .children(dialogs)
            .when_some(self.ledger_error.clone(), |panel, error| {
                panel.child(
                    div()
                        .debug_selector(|| "cedian-ledger-error".to_string())
                        .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
                )
            })
            .when_some(self.notice.clone(), |panel, notice| {
                panel.child(
                    div().debug_selector(|| "cedian-notice".to_string()).child(
                        Label::new(notice)
                            .size(LabelSize::Small)
                            .color(Color::Warning),
                    ),
                )
            })
            .when(images > 0, |panel| {
                panel.child(
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new(format!("{images} image(s) attached"))
                                .size(LabelSize::Small),
                        )
                        .child(
                            Button::new("cedian-clear-images", "Remove images").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.images.clear();
                                    cx.notify();
                                }),
                            ),
                        ),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(self.input.clone()))
                    .child(
                        Button::new("cedian-send", "Send")
                            .disabled(!idle && self.turn != Turn::Streaming)
                            .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
                    )
                    .when(self.turn == Turn::Streaming, |row| {
                        row.child(
                            div().debug_selector(|| "cedian-steer".to_string()).child(
                                Button::new("cedian-steer", "Steer")
                                    .on_click(cx.listener(|this, _, _, cx| this.steer(cx))),
                            ),
                        )
                    })
                    .when(stoppable, |row| {
                        row.child(
                            div().debug_selector(|| "cedian-stop".to_string()).child(
                                Button::new("cedian-stop", "Stop")
                                    .on_click(cx.listener(|this, _, _, cx| this.stop_turn(cx))),
                            ),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(footer)
                            .size(LabelSize::Small)
                            .color(if stopped || taken {
                                Color::Error
                            } else {
                                Color::Muted
                            }),
                    )
                    .when(taken, |row| {
                        row.child(
                            div()
                                .debug_selector(|| "cedian-new-session".to_string())
                                .child(
                                    Button::new("cedian-new-session", "Start a new session")
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.start_new_session(cx)
                                            }),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .debug_selector(|| "cedian-retry".to_string())
                                .child(Button::new("cedian-retry", "Retry").on_click(
                                    cx.listener(|this, _, _, cx| this.retry_session(cx)),
                                )),
                        )
                    })
                    .when(stopped, |row| {
                        row.child(
                            Button::new("cedian-restart", "Restart OMP").on_click(
                                cx.listener(|this, _, window, cx| this.restart(window, cx)),
                            ),
                        )
                    }),
            )
    }
}

impl Panel for CedianPanel {
    fn persistent_name() -> &'static str {
        "cedian"
    }

    fn panel_key() -> &'static str {
        PANEL_KEY
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(420.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::ZedAssistant)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("cedian")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        20
    }
}

#[cfg(test)]
mod tests {
    //! Live S9a exit check (T2 + T5) without a visible window: the real panel
    //! code, real OMP through the spawn profile, real disk, real Zed buffer
    //! undo. Needs OMP auth:
    //! `cargo test -p cedian_panel -- --ignored --nocapture live_`
    use super::*;
    use fs::Fs as _;
    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    use settings::SettingsStore;
    use std::time::{Duration, Instant};

    fn init(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    /// Pump the test executor while real OS threads (OMP) make progress.
    fn wait_until(
        cx: &mut TestAppContext,
        what: &str,
        mut done: impl FnMut(&mut TestAppContext) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn texts(panel: &CedianPanel) -> String {
        let (messages, cards) = cedian_agent_ui::render_thread(panel.thread.events());
        let mut out: Vec<String> = messages
            .iter()
            .map(|m| format!("{:?}: {}", m.role, m.text))
            .collect();
        out.extend(
            cards
                .iter()
                .map(|c| format!("card {:?} {}", c.status, c.display_line())),
        );
        out.join("\n")
    }

    /// ADR-0035: an audit failure fails the turn once, however many rows
    /// fail after it, and OMP settling brings the panel back to idle with
    /// the reason kept on screen.
    #[gpui::test]
    async fn an_audit_failure_fails_the_turn_once_and_settles_to_idle(cx: &mut TestAppContext) {
        init(cx);
        let project = Project::test(fs::FakeFs::new(cx.executor()), [], cx).await;
        let window = cx.add_window(|window, cx| CedianPanel::new(project.clone(), window, cx));
        window
            .update(cx, |panel, window, cx| {
                panel.turn = Turn::Streaming;
                for row in ["row 1", "row 2"] {
                    panel.on_link_event(LinkEvent::AuditFailed(row.to_string()), window, cx);
                }
                assert_eq!(panel.turn, Turn::Failed("audit: row 1".to_string()));
                panel.on_link_event(LinkEvent::Event(RouterEvent::Settled), window, cx);
                assert_eq!(panel.turn, Turn::Idle);
                assert_eq!(panel.notice(), Some("turn failed: audit: row 1"));
            })
            .unwrap();
    }

    /// A prompt call that ends in an error keeps what the person must know:
    /// a failed turn's reason, and a prompt OMP ran stays in the thread.
    #[gpui::test]
    async fn a_prompt_ending_in_an_error_keeps_the_failure_and_what_ran(cx: &mut TestAppContext) {
        init(cx);
        let project = Project::test(fs::FakeFs::new(cx.executor()), [], cx).await;
        let window = cx.add_window(|window, cx| CedianPanel::new(project.clone(), window, cx));
        window
            .update(cx, |panel, window, cx| {
                let failed = |panel: &mut CedianPanel| {
                    panel.turn = Turn::Failed("audit: row".to_string());
                    panel.notice = None;
                };
                let expected = Some("turn failed: audit: row");

                failed(panel);
                panel.on_link_event(LinkEvent::PromptFailed("pipe".into()), window, cx);
                assert_eq!((&panel.turn, panel.notice()), (&Turn::Idle, expected));

                failed(panel);
                panel.on_link_event(LinkEvent::PromptCancelled, window, cx);
                assert_eq!((&panel.turn, panel.notice()), (&Turn::Idle, expected));

                panel.thread.push_user("it ran");
                panel.turn = Turn::Stopping;
                panel.on_link_event(LinkEvent::PromptStopped("pipe".into()), window, cx);
                assert_eq!(panel.turn, Turn::Idle);
                assert_eq!(panel.notice(), Some("turn stopped: pipe"));
                assert!(texts(panel).contains("it ran"), "{}", texts(panel));
            })
            .unwrap();
    }

    const ORIGINAL: &str = "alpha\nbeta\ngamma\n";

    struct Fixture {
        fs: std::sync::Arc<fs::FakeFs>,
        project: Entity<Project>,
        window: gpui::WindowHandle<CedianPanel>,
    }

    /// A workspace with `notes.txt` open and `other.txt` on disk only; no
    /// OMP (the panel is driven with router events directly).
    async fn fixture(cx: &mut TestAppContext) -> (Fixture, Entity<Buffer>) {
        init(cx);
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/ws",
            serde_json::json!({"notes.txt": ORIGINAL, "other.txt": "one\ntwo\n"}),
        )
        .await;
        let project = Project::test(fs.clone(), [std::path::Path::new("/ws")], cx).await;
        let buffer = project
            .update(cx, |p, cx| p.open_local_buffer("/ws/notes.txt", cx))
            .await
            .unwrap();
        let window = cx.add_window(|window, cx| CedianPanel::new(project.clone(), window, cx));
        window
            .update(cx, |panel, _, _| {
                let state =
                    std::env::temp_dir().join(format!("cedian-panel-state-{}", std::process::id()));
                std::fs::create_dir_all(&state).unwrap();
                panel.set_state_dir(state);
                panel.review.begin_turn();
            })
            .unwrap();
        (
            Fixture {
                fs,
                project,
                window,
            },
            buffer,
        )
    }

    fn tool_start(f: &Fixture, cx: &mut TestAppContext, id: &str, paths: &[&str]) {
        tool_start_named(f, cx, id, "edit", paths);
    }

    fn tool_start_named(
        f: &Fixture,
        cx: &mut TestAppContext,
        id: &str,
        name: &str,
        paths: &[&str],
    ) {
        let event = RouterEvent::ToolStart {
            tool_call_id: id.to_string(),
            tool_name: name.to_string(),
            args_preview: String::new(),
            paths: paths.iter().map(|p| p.to_string()).collect(),
        };
        f.window
            .update(cx, |panel, _, cx| panel.on_event(event, cx))
            .unwrap();
    }

    fn settled(f: &Fixture, cx: &mut TestAppContext) {
        f.window
            .update(cx, |panel, _, cx| panel.on_event(RouterEvent::Settled, cx))
            .unwrap();
        cx.run_until_parked();
    }

    fn tool_end(f: &Fixture, cx: &mut TestAppContext, id: &str) {
        tool_end_with(f, cx, id, Vec::new());
    }

    fn tool_end_with(
        f: &Fixture,
        cx: &mut TestAppContext,
        id: &str,
        before: Vec<(String, cedian_omp::TextBefore)>,
    ) {
        tool_end_named(f, cx, id, "edit", before);
    }

    fn tool_end_named(
        f: &Fixture,
        cx: &mut TestAppContext,
        id: &str,
        name: &str,
        before: Vec<(String, cedian_omp::TextBefore)>,
    ) {
        let event = RouterEvent::ToolEnd {
            tool_call_id: id.to_string(),
            tool_name: name.to_string(),
            result_summary: String::new(),
            is_error: false,
            before,
        };
        f.window
            .update(cx, |panel, _, cx| panel.on_event(event, cx))
            .unwrap();
        cx.run_until_parked();
    }

    async fn omp_writes(f: &Fixture, path: &str, text: &str) {
        f.fs.save(
            std::path::Path::new(path),
            &text.into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();
    }

    /// `(path, status, tool calls)` per hunk, files in review order.
    fn hunks(f: &Fixture, cx: &mut TestAppContext) -> Vec<(String, HunkStatus, Vec<String>)> {
        f.window
            .update(cx, |panel, _, cx| {
                panel.review.rebuild(cx);
                panel
                    .review
                    .files()
                    .iter()
                    .flat_map(|file| {
                        let path = panel.review.path(file, cx).display().to_string();
                        file.hunks()
                            .iter()
                            .map(move |h| (path.clone(), h.status, h.tool_call_ids.clone()))
                    })
                    .collect()
            })
            .unwrap()
    }

    #[gpui::test]
    async fn an_omp_write_to_an_open_buffer_is_one_pending_hunk(cx: &mut TestAppContext) {
        let (f, buffer) = fixture(cx).await;
        tool_start(&f, cx, "c1", &["notes.txt"]);
        omp_writes(&f, "/ws/notes.txt", "alpha\nBETA\ngamma\n").await;
        tool_end(&f, cx, "c1");
        assert_eq!(
            hunks(&f, cx),
            vec![(
                "ws/notes.txt".to_string(),
                HunkStatus::Pending,
                vec!["c1".to_string()]
            )]
        );
        assert_eq!(
            buffer.read_with(cx, |b, _| b.text()),
            "alpha\nBETA\ngamma\n"
        );
    }

    /// Two calls write the same open file in sequence. The panel rebuilds
    /// the review on every buffer edit, so the second import lands in the
    /// review before it is attributed; it is still the agent's, not a user
    /// edit over the first hunk.
    #[gpui::test]
    async fn two_calls_to_one_file_are_both_pending(cx: &mut TestAppContext) {
        for watcher_first in [false, true] {
            let (f, buffer) = fixture(cx).await;
            tool_start(&f, cx, "c1", &["notes.txt"]);
            omp_writes(&f, "/ws/notes.txt", "alpha\nBETA\ngamma\n").await;
            tool_end(&f, cx, "c1");
            tool_start(&f, cx, "c2", &["notes.txt"]);
            omp_writes(&f, "/ws/notes.txt", "alpha\nBETA\nGAMMA\n").await;
            if watcher_first {
                cx.run_until_parked();
                assert_eq!(
                    buffer.read_with(cx, |b, _| b.text()),
                    "alpha\nBETA\nGAMMA\n",
                    "the watcher reloaded before the tool ended"
                );
            }
            tool_end(&f, cx, "c2");
            assert_eq!(
                hunks(&f, cx),
                vec![
                    (
                        "ws/notes.txt".to_string(),
                        HunkStatus::Pending,
                        vec!["c1".to_string()]
                    ),
                    (
                        "ws/notes.txt".to_string(),
                        HunkStatus::Pending,
                        vec!["c2".to_string()]
                    ),
                ],
                "watcher first: {watcher_first}"
            );
            let events = f
                .window
                .update(cx, |panel, _, _| panel.review.drain_events())
                .unwrap();
            assert!(events.is_empty(), "no user edit was reported: {events:?}");
        }
    }

    #[gpui::test]
    async fn a_file_omp_writes_that_is_not_open_is_opened_and_reviewed(cx: &mut TestAppContext) {
        let (f, _buffer) = fixture(cx).await;
        tool_start(&f, cx, "c1", &["other.txt"]);
        cx.run_until_parked();
        omp_writes(&f, "/ws/other.txt", "ONE\ntwo\n").await;
        tool_end(&f, cx, "c1");
        assert_eq!(
            hunks(&f, cx),
            vec![(
                "ws/other.txt".to_string(),
                HunkStatus::Pending,
                vec!["c1".to_string()]
            )]
        );
        let (old, new) = f
            .window
            .update(cx, |panel, _, _| {
                let h = &panel.review.files()[0].hunks()[0];
                (h.old_text.clone(), h.new_text.clone())
            })
            .unwrap();
        assert_eq!((old.as_str(), new.as_str()), ("one\n", "ONE\n"));
        let open = f.project.read_with(cx, |p, cx| p.opened_buffers(cx).len());
        assert_eq!(open, 2, "other.txt is open now");
    }

    /// The write lands before the file is open: the disk text at the tool's
    /// start is the baseline, so the write is still one pending hunk.
    #[gpui::test]
    async fn a_write_landing_before_the_open_is_still_a_hunk(cx: &mut TestAppContext) {
        let (f, _buffer) = fixture(cx).await;
        tool_start(&f, cx, "c1", &["other.txt"]);
        cx.run_until_parked();
        let open = f.project.read_with(cx, |p, cx| p.opened_buffers(cx).len());
        assert_eq!(open, 1, "the file is not opened before the write lands");
        omp_writes(&f, "/ws/other.txt", "ONE\ntwo\n").await;
        tool_end(&f, cx, "c1");
        let got = hunks(&f, cx);
        assert_eq!(
            got,
            vec![(
                "ws/other.txt".to_string(),
                HunkStatus::Pending,
                vec!["c1".to_string()]
            )]
        );
        let (old, new) = f
            .window
            .update(cx, |panel, _, _| {
                let h = &panel.review.files()[0].hunks()[0];
                (h.old_text.clone(), h.new_text.clone())
            })
            .unwrap();
        assert_eq!((old.as_str(), new.as_str()), ("one\n", "ONE\n"));
    }

    /// OMP does not wait for cedian: its write can land before the panel
    /// reads the file's disk text. OMP's own `oldText` is then the
    /// baseline, so the write is still one pending hunk.
    #[gpui::test]
    async fn a_write_that_beat_the_read_imports_from_omps_old_text(cx: &mut TestAppContext) {
        let (f, _buffer) = fixture(cx).await;
        omp_writes(&f, "/ws/other.txt", "ONE\ntwo\n").await;
        tool_start(&f, cx, "c1", &["other.txt"]);
        cx.run_until_parked();
        let before = cedian_omp::TextBefore::Text("one\ntwo\n".to_string());
        tool_end_with(&f, cx, "c1", vec![("/ws/other.txt".to_string(), before)]);
        assert_eq!(
            hunks(&f, cx),
            vec![(
                "ws/other.txt".to_string(),
                HunkStatus::Pending,
                vec!["c1".to_string()]
            )]
        );
        let (old, new) = f
            .window
            .update(cx, |panel, _, _| {
                let h = &panel.review.files()[0].hunks()[0];
                (h.old_text.clone(), h.new_text.clone())
            })
            .unwrap();
        assert_eq!((old.as_str(), new.as_str()), ("one\n", "ONE\n"));
    }

    /// The same race when OMP pruned its before-text (past 32 KiB) or its
    /// tool reports none (`write`): the file is listed as not reviewable,
    /// never silently Unchanged.
    async fn raced_without_old_text(
        cx: &mut TestAppContext,
        before: Vec<(String, cedian_omp::TextBefore)>,
    ) {
        let (f, _buffer) = fixture(cx).await;
        omp_writes(&f, "/ws/other.txt", "ONE\ntwo\n").await;
        tool_start(&f, cx, "c1", &["other.txt"]);
        cx.run_until_parked();
        tool_end_with(&f, cx, "c1", before);
        assert!(hunks(&f, cx).is_empty());
        let unreviewable = f
            .window
            .update(cx, |panel, _, _| panel.review.unreviewable().to_vec())
            .unwrap();
        assert_eq!(unreviewable.len(), 1, "{unreviewable:?}");
        assert_eq!(unreviewable[0].0, PathBuf::from("ws/other.txt"));
        assert!(unreviewable[0].1.contains("c1"), "{unreviewable:?}");
    }

    #[gpui::test]
    async fn a_write_that_beat_the_read_with_pruned_old_text_is_not_reviewable(
        cx: &mut TestAppContext,
    ) {
        let pruned = cedian_omp::TextBefore::Pruned;
        raced_without_old_text(cx, vec![("/ws/other.txt".to_string(), pruned)]).await;
    }

    #[gpui::test]
    async fn a_write_that_beat_the_read_with_no_old_text_is_not_reviewable(
        cx: &mut TestAppContext,
    ) {
        raced_without_old_text(cx, Vec::new()).await;
    }

    fn unreviewable(f: &Fixture, cx: &mut TestAppContext) -> Vec<(PathBuf, String)> {
        f.window
            .update(cx, |panel, _, _| panel.review.unreviewable().to_vec())
            .unwrap()
    }

    /// ADR-0047: an open buffer a `bash` call changes on disk is that
    /// call's hunk.
    #[gpui::test]
    async fn a_bash_write_to_an_open_buffer_is_that_calls_hunk(cx: &mut TestAppContext) {
        let (f, _buffer) = fixture(cx).await;
        tool_start_named(&f, cx, "b1", "bash", &[]);
        cx.run_until_parked();
        omp_writes(&f, "/ws/notes.txt", "alpha\nBETA\ngamma\n").await;
        cx.run_until_parked();
        tool_end_named(&f, cx, "b1", "bash", Vec::new());
        assert_eq!(
            hunks(&f, cx),
            vec![(
                "ws/notes.txt".to_string(),
                HunkStatus::Pending,
                vec!["b1".to_string()]
            )]
        );
        assert!(unreviewable(&f, cx).is_empty());
    }

    /// A command that exits non-zero may still have written: its changes
    /// are its hunks too.
    #[gpui::test]
    async fn a_failed_bash_calls_writes_are_still_its_hunks(cx: &mut TestAppContext) {
        let (f, _buffer) = fixture(cx).await;
        tool_start_named(&f, cx, "b1", "bash", &[]);
        cx.run_until_parked();
        omp_writes(&f, "/ws/notes.txt", "alpha\nBETA\ngamma\n").await;
        cx.run_until_parked();
        let event = RouterEvent::ToolEnd {
            tool_call_id: "b1".to_string(),
            tool_name: "bash".to_string(),
            result_summary: "exit 1".to_string(),
            is_error: true,
            before: Vec::new(),
        };
        f.window
            .update(cx, |panel, _, cx| panel.on_event(event, cx))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(hunks(&f, cx).len(), 1);
    }

    /// A buffer the person had unsaved edits in is never overwritten by a
    /// bash write: it shows STALE with the call, their text kept.
    #[gpui::test]
    async fn a_bash_write_under_unsaved_edits_is_stale(cx: &mut TestAppContext) {
        let (f, buffer) = fixture(cx).await;
        buffer.update(cx, |b, cx| b.edit([(0..0, "USER ")], None, cx));
        tool_start_named(&f, cx, "b1", "bash", &[]);
        cx.run_until_parked();
        omp_writes(&f, "/ws/notes.txt", "alpha\nBETA\ngamma\n").await;
        cx.run_until_parked();
        tool_end_named(&f, cx, "b1", "bash", Vec::new());
        let reason = f
            .window
            .update(cx, |panel, _, _| {
                panel.review.files()[0].stale_import().map(str::to_string)
            })
            .unwrap();
        assert!(
            reason.as_ref().is_some_and(|r| r.contains("b1")),
            "{reason:?}"
        );
        assert_eq!(
            buffer.read_with(cx, |b, _| b.text()),
            "USER alpha\nbeta\ngamma\n"
        );
    }

    /// A file not open has no text from before the call: it is listed as
    /// changed by that call, not reviewable as hunks (owner, ADR-0055).
    #[gpui::test]
    async fn a_bash_write_to_a_file_not_open_is_listed(cx: &mut TestAppContext) {
        let (f, _buffer) = fixture(cx).await;
        tool_start_named(&f, cx, "b1", "bash", &[]);
        cx.run_until_parked();
        omp_writes(&f, "/ws/other.txt", "ONE\ntwo\n").await;
        cx.run_until_parked();
        tool_end_named(&f, cx, "b1", "bash", Vec::new());
        assert!(hunks(&f, cx).is_empty());
        let listed = unreviewable(&f, cx);
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].0, PathBuf::from("ws/other.txt"));
        assert!(listed[0].1.contains("bash call b1"), "{listed:?}");
    }

    /// Another mutating call writing the same file while a bash call runs:
    /// which call made which change is unknown, so neither imports it and
    /// the file is shown as such (ADR-0055 decision 6).
    #[gpui::test]
    async fn an_edit_overlapping_a_bash_call_imports_nothing_for_the_file(cx: &mut TestAppContext) {
        let (f, _buffer) = fixture(cx).await;
        tool_start_named(&f, cx, "b1", "bash", &[]);
        tool_start(&f, cx, "e1", &["notes.txt"]);
        cx.run_until_parked();
        omp_writes(&f, "/ws/notes.txt", "alpha\nBETA\ngamma\n").await;
        cx.run_until_parked();
        tool_end(&f, cx, "e1");
        tool_end_named(&f, cx, "b1", "bash", Vec::new());
        assert!(hunks(&f, cx).is_empty(), "{:?}", hunks(&f, cx));
        let listed = unreviewable(&f, cx);
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert!(
            listed[0].1.contains("b1") && listed[0].1.contains("e1"),
            "{listed:?}"
        );
    }

    /// An open buffer with unsaved edits, a write that beat cedian's disk
    /// read, and no text from OMP (`write` reports none): the file is listed
    /// as changed on disk under the person's edits, never Unchanged, and
    /// their text is kept.
    async fn dirty_raced_without_old_text(
        cx: &mut TestAppContext,
        before: Vec<(String, cedian_omp::TextBefore)>,
    ) {
        let (f, buffer) = fixture(cx).await;
        buffer.update(cx, |b, cx| b.edit([(0..0, "USER ")], None, cx));
        omp_writes(&f, "/ws/notes.txt", "alpha\nBETA\ngamma\n").await;
        tool_start(&f, cx, "e1", &["notes.txt"]);
        cx.run_until_parked();
        tool_end_with(&f, cx, "e1", before);
        assert!(hunks(&f, cx).is_empty());
        let listed = unreviewable(&f, cx);
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert!(
            listed[0].1.contains("e1") && listed[0].1.contains("unsaved"),
            "{listed:?}"
        );
        assert_eq!(
            buffer.read_with(cx, |b, _| b.text()),
            "USER alpha\nbeta\ngamma\n"
        );
    }

    #[gpui::test]
    async fn a_raced_write_under_unsaved_edits_with_no_old_text_is_listed(cx: &mut TestAppContext) {
        dirty_raced_without_old_text(cx, Vec::new()).await;
    }

    #[gpui::test]
    async fn a_raced_write_under_unsaved_edits_with_pruned_old_text_is_listed(
        cx: &mut TestAppContext,
    ) {
        let pruned = cedian_omp::TextBefore::Pruned;
        dirty_raced_without_old_text(cx, vec![("/ws/notes.txt".to_string(), pruned)]).await;
    }

    /// Zed's watcher reloaded a clean open buffer before cedian marked it:
    /// OMP's `oldText` shows the mark is after the write, so the file is
    /// never silently Unchanged.
    #[gpui::test]
    async fn a_clean_buffer_reloaded_before_the_mark_is_not_unchanged(cx: &mut TestAppContext) {
        let (f, buffer) = fixture(cx).await;
        omp_writes(&f, "/ws/notes.txt", "alpha\nBETA\ngamma\n").await;
        cx.run_until_parked();
        assert_eq!(
            buffer.read_with(cx, |b, _| b.text()),
            "alpha\nBETA\ngamma\n"
        );
        tool_start(&f, cx, "e1", &["notes.txt"]);
        cx.run_until_parked();
        let before = cedian_omp::TextBefore::Text(ORIGINAL.to_string());
        tool_end_with(&f, cx, "e1", vec![("/ws/notes.txt".to_string(), before)]);
        let got = hunks(&f, cx);
        let listed = unreviewable(&f, cx);
        assert!(got.is_empty(), "{got:?}");
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert!(
            listed[0].1.contains("before cedian marked it"),
            "{listed:?}"
        );
    }

    /// A CRLF `oldText` is the same text as the LF disk text: a call that
    /// left the file alone is no STALE under the person's unsaved edits.
    #[gpui::test]
    async fn a_crlf_old_text_is_normalized(cx: &mut TestAppContext) {
        let (f, buffer) = fixture(cx).await;
        buffer.update(cx, |b, cx| b.edit([(0..0, "USER ")], None, cx));
        tool_start(&f, cx, "c1", &["notes.txt"]);
        cx.run_until_parked();
        let before = cedian_omp::TextBefore::Text(ORIGINAL.replace('\n', "\r\n"));
        tool_end_with(&f, cx, "c1", vec![("/ws/notes.txt".to_string(), before)]);
        let stale = f
            .window
            .update(cx, |panel, _, _| {
                panel
                    .review
                    .files()
                    .iter()
                    .find_map(|f| f.stale_import().map(str::to_string))
            })
            .unwrap();
        assert_eq!(stale, None);
        assert!(unreviewable(&f, cx).is_empty());
    }

    /// A move's before-text is its source's: it is never imported from it.
    #[gpui::test]
    async fn a_moved_file_is_not_reviewable(cx: &mut TestAppContext) {
        let (f, _buffer) = fixture(cx).await;
        tool_start(&f, cx, "c1", &["other.txt"]);
        cx.run_until_parked();
        omp_writes(&f, "/ws/other.txt", "ONE\ntwo\n").await;
        let moved = cedian_omp::TextBefore::Moved;
        tool_end_with(&f, cx, "c1", vec![("/ws/other.txt".to_string(), moved)]);
        assert!(hunks(&f, cx).is_empty(), "{:?}", hunks(&f, cx));
        let listed = unreviewable(&f, cx);
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert!(listed[0].1.contains("moved"), "{listed:?}");
    }

    async fn agent_hunk(f: &Fixture, cx: &mut TestAppContext, id: &str, text: &str) {
        tool_start(f, cx, id, &["notes.txt"]);
        cx.run_until_parked();
        omp_writes(f, "/ws/notes.txt", text).await;
        tool_end(f, cx, id);
    }

    /// The overlap path imports nothing, and the watcher's reload of the
    /// agents' write is no edit of the person's: an earlier hunk does not
    /// go STALE and no `user_edited_agent_hunk` row is written.
    #[gpui::test]
    async fn an_overlap_does_not_credit_the_agents_write_to_the_person(cx: &mut TestAppContext) {
        let (f, _buffer) = fixture(cx).await;
        agent_hunk(&f, cx, "e0", "alpha\nBETA\ngamma\n").await;
        f.window
            .update(cx, |panel, _, _| panel.review.drain_events())
            .unwrap();
        tool_start_named(&f, cx, "b1", "bash", &[]);
        tool_start(&f, cx, "e1", &["notes.txt"]);
        cx.run_until_parked();
        omp_writes(&f, "/ws/notes.txt", "alpha\nBETA2\ngamma\n").await;
        cx.run_until_parked();
        tool_end(&f, cx, "e1");
        tool_end_named(&f, cx, "b1", "bash", Vec::new());
        let got = hunks(&f, cx);
        assert!(
            got.iter()
                .all(|(_, status, _)| *status != HunkStatus::Stale),
            "{got:?}"
        );
        let events = f
            .window
            .update(cx, |panel, _, _| panel.review.drain_events())
            .unwrap();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, crate::review::ReviewEvent::UserEditedAgentHunk { .. })),
            "{events:?}"
        );
        assert_eq!(unreviewable(&f, cx).len(), 1);
    }

    /// Only files are listed, never a directory, an ignored path or the
    /// worktree's initial scan; and the call is named as the time, not the
    /// writer.
    #[gpui::test]
    async fn bash_lists_only_files_that_changed_while_it_ran(cx: &mut TestAppContext) {
        let (f, _buffer) = fixture(cx).await;
        omp_writes(&f, "/ws/.gitignore", "target/\n").await;
        f.fs.create_dir(std::path::Path::new("/ws/target"))
            .await
            .unwrap();
        cx.run_until_parked();
        tool_start_named(&f, cx, "b1", "bash", &[]);
        cx.run_until_parked();
        f.fs.create_dir(std::path::Path::new("/ws/sub"))
            .await
            .unwrap();
        omp_writes(&f, "/ws/sub/new.txt", "n\n").await;
        omp_writes(&f, "/ws/target/out.txt", "o\n").await;
        cx.run_until_parked();
        tool_end_named(&f, cx, "b1", "bash", Vec::new());
        let listed = unreviewable(&f, cx);
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].0, PathBuf::from("ws/sub/new.txt"));
        assert!(listed[0].1.contains("while bash call b1 ran"), "{listed:?}");
    }

    /// A running bash call does not freeze the review: a hunk can still be
    /// rejected, and Accept all still runs.
    #[gpui::test]
    async fn a_running_bash_call_does_not_block_review_actions(cx: &mut TestAppContext) {
        let (f, _buffer) = fixture(cx).await;
        agent_hunk(&f, cx, "e0", "alpha\nBETA\ngamma\nDELTA\n").await;
        tool_start_named(&f, cx, "b1", "bash", &[]);
        cx.run_until_parked();
        let refused = f
            .window
            .update(cx, |panel, _, cx| {
                panel.review.rebuild(cx);
                let path = PathBuf::from("ws/notes.txt");
                let key = panel.review.files()[0].hunks()[0].key.clone();
                let reject = panel.review.reject(&path, &key, cx).err();
                let all = panel.review.accept_all(false, cx).err();
                (reject, all)
            })
            .unwrap();
        assert!(refused.0.is_none() && refused.1.is_none(), "{refused:?}");
    }

    /// OMP dies after a refused `cedian_complete` and before the turn
    /// settles: stopping still ends the workflow's turn, so the refusal
    /// blocks it now, in this turn.
    #[gpui::test]
    async fn stopping_ends_the_workflow_turn(cx: &mut TestAppContext) {
        let (f, _buffer) = fixture(cx).await;
        let dir = std::env::temp_dir().join(format!("cedian-panel-stop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut state = cedian_workflow::WorkflowState::start(cedian_workflow::TaskProfile::new(
            "t",
            cedian_workflow::TaskKind::BugFix,
        ))
        .unwrap();
        state.last_completion = Some(cedian_workflow::CompletionAttempt {
            claims: Vec::new(),
            accepted: false,
            missing: vec!["verify".to_string()],
            turn_ended: false,
        });
        cedian_shell::workflow_store::save(&dir, &state).unwrap();
        f.window
            .update(cx, |panel, _, cx| {
                panel.set_state_dir(dir.clone());
                panel.stop("OMP exited".to_string(), cx);
            })
            .unwrap();
        let status = cedian_shell::workflow_store::load(&dir).unwrap().status;
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(status, cedian_workflow::WorkflowStatus::Blocked);
    }

    /// The correction ledger failing again and again says so once.
    #[gpui::test]
    async fn a_failing_ledger_is_one_notice(cx: &mut TestAppContext) {
        let (f, _buffer) = fixture(cx).await;
        agent_hunk(&f, cx, "e0", "alpha\nBETA\ngamma\nDELTA\n").await;
        let notice = f
            .window
            .update(cx, |panel, _, cx| {
                let file = std::env::temp_dir()
                    .join(format!("cedian-panel-not-a-dir-{}", std::process::id()));
                std::fs::write(&file, "").unwrap();
                panel.set_state_dir(file);
                panel.review.rebuild(cx);
                let path = PathBuf::from("ws/notes.txt");
                for _ in 0..2 {
                    let key = panel.review.files()[0].hunks()[0].key.clone();
                    panel.reject_hunk(&path, &key, cx);
                }
                format!("{:?} {:?}", panel.notice(), panel.ledger_error)
            })
            .unwrap();
        assert_eq!(notice.matches("did not record").count(), 1, "{notice}");
    }

    /// The person opens the file and types between the tool's start and its
    /// end: their buffer is kept as it is, dirty, and the file shows STALE
    /// with the reason.
    #[gpui::test]
    async fn a_file_the_user_opened_and_typed_in_during_the_call_keeps_their_text(
        cx: &mut TestAppContext,
    ) {
        let (f, _notes) = fixture(cx).await;
        tool_start(&f, cx, "c1", &["other.txt"]);
        cx.run_until_parked();
        let other = f
            .project
            .update(cx, |p, cx| p.open_local_buffer("/ws/other.txt", cx))
            .await
            .unwrap();
        other.update(cx, |b, cx| b.edit([(0..0, "USER ")], None, cx));
        omp_writes(&f, "/ws/other.txt", "ONE\ntwo\n").await;
        tool_end(&f, cx, "c1");
        other.read_with(cx, |b, _| {
            assert_eq!(b.text(), "USER one\ntwo\n", "the user's text is kept");
            assert!(b.is_dirty(), "and still unsaved");
        });
        let (stale, unreviewable) = f
            .window
            .update(cx, |panel, _, _| {
                (
                    panel
                        .review
                        .files()
                        .iter()
                        .filter_map(|f| f.stale_import().map(str::to_string))
                        .collect::<Vec<_>>(),
                    panel.review.unreviewable().to_vec(),
                )
            })
            .unwrap();
        assert_eq!(
            stale,
            vec![
                "the file changed during call c1 while you were editing it \
                 (unsaved edits, or a save during the call); nothing was imported"
                    .to_string()
            ],
            "the file shows STALE with the reason"
        );
        assert!(unreviewable.is_empty(), "{unreviewable:?}");
    }

    /// A write that creates a file: one all-added hunk; reject empties it.
    #[gpui::test]
    async fn a_new_file_is_one_added_hunk_and_reject_empties_it(cx: &mut TestAppContext) {
        let (f, _buffer) = fixture(cx).await;
        tool_start(&f, cx, "c1", &["fresh.txt"]);
        cx.run_until_parked();
        omp_writes(&f, "/ws/fresh.txt", "hello\nworld\n").await;
        tool_end(&f, cx, "c1");
        let got = hunks(&f, cx);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, "ws/fresh.txt");
        let (path, key, old, new) = f
            .window
            .update(cx, |panel, _, cx| {
                let file = &panel.review.files()[0];
                let h = &file.hunks()[0];
                (
                    panel.review.path(file, cx),
                    h.key.clone(),
                    h.old_text.clone(),
                    h.new_text.clone(),
                )
            })
            .unwrap();
        assert_eq!((old.as_str(), new.as_str()), ("", "hello\nworld\n"));
        let buffer = f
            .window
            .update(cx, |panel, _, _| panel.review.files()[0].buffer().clone())
            .unwrap();
        f.window
            .update(cx, |panel, _, cx| panel.reject_hunk(&path, &key, cx))
            .unwrap();
        assert_eq!(buffer.read_with(cx, |b, _| b.text()), "");
    }

    /// A file OMP wrote that could not be opened or imported is still shown.
    #[gpui::test]
    async fn a_file_that_could_not_be_reviewed_is_shown(cx: &mut TestAppContext) {
        let (f, _buffer) = fixture(cx).await;
        f.window
            .update(cx, |panel, _, cx| {
                panel
                    .review
                    .could_not_review(PathBuf::from("/ws/out.txt"), "permission denied".into());
                assert!(!panel.review.is_empty());
                cx.notify();
            })
            .unwrap();
        let mut vcx = VisualTestContext::from_window(f.window.into(), cx);
        click(&mut vcx, "cedian-review-toggle");
        assert!(
            vcx.debug_bounds("cedian-unreviewed-/ws/out.txt").is_some(),
            "the file is on screen with its reason"
        );
    }

    #[gpui::test]
    async fn a_file_that_cannot_be_opened_is_shown_as_unreviewed(cx: &mut TestAppContext) {
        let (f, _buffer) = fixture(cx).await;
        tool_start(&f, cx, "c1", &["/elsewhere/out.txt"]);
        cx.run_until_parked();
        tool_end(&f, cx, "c1");
        let shown = f
            .window
            .update(cx, |panel, _, _| panel.review.unreviewable().to_vec())
            .unwrap();
        assert_eq!(shown.len(), 1, "{shown:?}");
        assert_eq!(shown[0].0, PathBuf::from("/elsewhere/out.txt"));
        let empty = f
            .window
            .update(cx, |panel, _, _| panel.review.is_empty())
            .unwrap();
        assert!(!empty, "the file shows in review");
    }

    #[gpui::test]
    async fn a_refused_import_shows_the_file_stale(cx: &mut TestAppContext) {
        let (f, buffer) = fixture(cx).await;
        buffer.update(cx, |b, cx| b.edit([(0..0, "USER ")], None, cx));
        tool_start(&f, cx, "c1", &["notes.txt"]);
        cx.run_until_parked();
        omp_writes(&f, "/ws/notes.txt", "alpha\nBETA\ngamma\n").await;
        tool_end(&f, cx, "c1");
        let reason = f
            .window
            .update(cx, |panel, _, _| {
                panel.review.files()[0].stale_import().map(str::to_string)
            })
            .unwrap();
        assert!(
            reason.as_ref().is_some_and(|r| r.contains("c1")),
            "{reason:?}"
        );
        assert_eq!(
            buffer.read_with(cx, |b, _| b.text()),
            "USER alpha\nbeta\ngamma\n"
        );
    }

    fn click(vcx: &mut VisualTestContext, selector: &str) {
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        let bounds = vcx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is not on screen"));
        vcx.simulate_click(bounds.center(), Modifiers::none());
        vcx.run_until_parked();
    }

    #[gpui::test]
    async fn reject_and_accept_buttons_resolve_hunks(cx: &mut TestAppContext) {
        let (f, buffer) = fixture(cx).await;
        tool_start(&f, cx, "c1", &["notes.txt"]);
        omp_writes(&f, "/ws/notes.txt", "ALPHA\nbeta\nGAMMA\n").await;
        tool_end(&f, cx, "c1");
        let mut vcx = VisualTestContext::from_window(f.window.into(), cx);
        click(&mut vcx, "cedian-review-toggle");
        assert!(
            vcx.debug_bounds("cedian-review").is_some(),
            "the view is open"
        );
        click(&mut vcx, "cedian-reject-ws/notes.txt-0");
        assert_eq!(
            buffer.read_with(&vcx, |b, _| b.text()),
            "alpha\nbeta\nGAMMA\n",
            "reject put the baseline line back"
        );
        click(&mut vcx, "cedian-accept-ws/notes.txt-0");
        let statuses: Vec<HunkStatus> = hunks(&f, &mut vcx).into_iter().map(|h| h.1).collect();
        assert_eq!(statuses, vec![HunkStatus::Accepted]);
        assert!(
            vcx.debug_bounds("cedian-accept-ws/notes.txt-0").is_none(),
            "a resolved hunk has no buttons"
        );
        buffer.update(&mut vcx, |b, cx| {
            b.undo(cx);
            assert_eq!(
                b.text(),
                "ALPHA\nbeta\nGAMMA\n",
                "one undo restores the reject"
            );
        });
    }

    /// The person restores hunk A by hand and then acts on the Reject they
    /// saw on it: the click carries A's identity, so nothing happens to B,
    /// which now sits where A was. (The harness redraws on the edit, so the
    /// stale frame's handler is invoked directly.)
    #[gpui::test]
    async fn a_click_on_a_hunk_that_moved_does_nothing_to_the_next_one(cx: &mut TestAppContext) {
        let (f, buffer) = fixture(cx).await;
        tool_start(&f, cx, "c1", &["notes.txt"]);
        omp_writes(&f, "/ws/notes.txt", "ALPHA\nbeta\nGAMMA\n").await;
        tool_end(&f, cx, "c1");
        let mut vcx = VisualTestContext::from_window(f.window.into(), cx);
        click(&mut vcx, "cedian-review-toggle");
        assert!(vcx.debug_bounds("cedian-reject-ws/notes.txt-1").is_some());
        let (path, key_a) = f
            .window
            .update(&mut vcx, |panel, _, cx| {
                let file = &panel.review.files()[0];
                (panel.review.path(file, cx), file.hunks()[0].key.clone())
            })
            .unwrap();
        buffer.update(&mut vcx, |b, cx| b.edit([(0..5, "alpha")], None, cx));
        vcx.run_until_parked();
        assert!(
            vcx.debug_bounds("cedian-reject-ws/notes.txt-1").is_none(),
            "the view shows what is there: one hunk left"
        );
        f.window
            .update(&mut vcx, |panel, _, cx| {
                panel.reject_hunk(&path, &key_a, cx)
            })
            .unwrap();
        assert_eq!(
            buffer.read_with(&vcx, |b, _| b.text()),
            "alpha\nbeta\nGAMMA\n",
            "B is untouched"
        );
        let hunks = hunks(&f, &mut vcx);
        assert_eq!(hunks.len(), 1, "{hunks:?}");
        assert_eq!(hunks[0].1, HunkStatus::Pending);
        let notice = f
            .window
            .update(&mut vcx, |panel, _, _| panel.notice().map(str::to_string))
            .unwrap();
        assert!(
            notice
                .as_deref()
                .is_some_and(|n| n.contains("moved; review again")),
            "{notice:?}"
        );
    }

    /// Accept all with STALE hunks in the set asks first; Yes takes them
    /// too, No takes only the others (owner ruling 2026-10-08).
    async fn accept_all_with_stale(cx: &mut TestAppContext, answer: &str) -> Vec<HunkStatus> {
        let (f, buffer) = fixture(cx).await;
        tool_start(&f, cx, "c1", &["notes.txt"]);
        omp_writes(&f, "/ws/notes.txt", "ALPHA\nbeta\nGAMMA\n").await;
        tool_end(&f, cx, "c1");
        buffer.update(cx, |b, cx| b.edit([(11..11, "!")], None, cx));
        let mut vcx = VisualTestContext::from_window(f.window.into(), cx);
        click(&mut vcx, "cedian-review-toggle");
        assert!(vcx.debug_bounds("cedian-accept-all-confirm").is_none());
        click(&mut vcx, "cedian-accept-all");
        assert!(
            vcx.debug_bounds("cedian-accept-all-confirm").is_some(),
            "asked before anything is accepted"
        );
        let before: Vec<HunkStatus> = hunks(&f, &mut vcx).into_iter().map(|h| h.1).collect();
        assert_eq!(before, vec![HunkStatus::Pending, HunkStatus::Stale]);
        click(&mut vcx, answer);
        assert!(vcx.debug_bounds("cedian-accept-all-confirm").is_none());
        hunks(&f, &mut vcx).into_iter().map(|h| h.1).collect()
    }

    #[gpui::test]
    async fn accept_all_yes_takes_the_stale_hunks_too(cx: &mut TestAppContext) {
        assert_eq!(
            accept_all_with_stale(cx, "cedian-accept-all-yes").await,
            vec![HunkStatus::Accepted, HunkStatus::Accepted]
        );
    }

    #[gpui::test]
    async fn accept_all_no_leaves_the_stale_hunks(cx: &mut TestAppContext) {
        assert_eq!(
            accept_all_with_stale(cx, "cedian-accept-all-no").await,
            vec![HunkStatus::Accepted, HunkStatus::Stale]
        );
    }

    /// The question is about the STALE hunks there are: when the person
    /// accepts the one STALE hunk while it is open, there is nothing to ask.
    #[gpui::test]
    async fn the_accept_all_question_goes_when_no_hunk_is_stale(cx: &mut TestAppContext) {
        let (f, buffer) = fixture(cx).await;
        tool_start(&f, cx, "c1", &["notes.txt"]);
        omp_writes(&f, "/ws/notes.txt", "ALPHA\nbeta\nGAMMA\n").await;
        tool_end(&f, cx, "c1");
        buffer.update(cx, |b, cx| b.edit([(11..11, "!")], None, cx));
        let mut vcx = VisualTestContext::from_window(f.window.into(), cx);
        click(&mut vcx, "cedian-review-toggle");
        click(&mut vcx, "cedian-accept-all");
        assert!(vcx.debug_bounds("cedian-accept-all-confirm").is_some());
        click(&mut vcx, "cedian-accept-ws/notes.txt-1");
        assert!(
            vcx.debug_bounds("cedian-accept-all-confirm").is_none(),
            "no STALE hunk is left to ask about"
        );
        let statuses: Vec<HunkStatus> = hunks(&f, &mut vcx).into_iter().map(|h| h.1).collect();
        assert_eq!(statuses, vec![HunkStatus::Pending, HunkStatus::Accepted]);
    }

    #[gpui::test]
    async fn accept_all_without_stale_does_not_ask(cx: &mut TestAppContext) {
        let (f, _buffer) = fixture(cx).await;
        tool_start(&f, cx, "c1", &["notes.txt"]);
        omp_writes(&f, "/ws/notes.txt", "ALPHA\nbeta\nGAMMA\n").await;
        tool_end(&f, cx, "c1");
        let mut vcx = VisualTestContext::from_window(f.window.into(), cx);
        click(&mut vcx, "cedian-review-toggle");
        click(&mut vcx, "cedian-accept-all");
        assert!(vcx.debug_bounds("cedian-accept-all-confirm").is_none());
        let after: Vec<HunkStatus> = hunks(&f, &mut vcx).into_iter().map(|h| h.1).collect();
        assert_eq!(after, vec![HunkStatus::Accepted, HunkStatus::Accepted]);
    }

    #[gpui::test]
    async fn the_revert_turn_button_puts_the_turn_back(cx: &mut TestAppContext) {
        let (f, buffer) = fixture(cx).await;
        tool_start(&f, cx, "c1", &["notes.txt"]);
        omp_writes(&f, "/ws/notes.txt", "ALPHA\nbeta\ngamma\n").await;
        tool_end(&f, cx, "c1");
        let mut vcx = VisualTestContext::from_window(f.window.into(), cx);
        click(&mut vcx, "cedian-review-toggle");
        settled(&f, &mut vcx);
        click(&mut vcx, "cedian-revert-turn");
        assert_eq!(buffer.read_with(&vcx, |b, _| b.text()), ORIGINAL);
        let notice = f
            .window
            .update(&mut vcx, |panel, _, _| panel.notice().map(str::to_string))
            .unwrap();
        assert_eq!(
            notice.as_deref(),
            Some(
                "turn 1 reverted: 1 hunk(s) put back, 0 STALE kept, \
                 0 changed again by a later turn, kept, 0 accepted, kept"
            )
        );
    }

    /// A prompt that did not settle (stopped, cancelled, failed, or the
    /// session taken) still ends the review's turn, so Revert is not stuck
    /// on "turn n is still running".
    #[gpui::test]
    async fn a_stopped_prompt_ends_the_reviews_turn(cx: &mut TestAppContext) {
        let (f, buffer) = fixture(cx).await;
        tool_start(&f, cx, "c1", &["notes.txt"]);
        omp_writes(&f, "/ws/notes.txt", "ALPHA\nbeta\ngamma\n").await;
        tool_end(&f, cx, "c1");
        f.window
            .update(cx, |panel, window, cx| {
                panel.turn = Turn::Stopping;
                panel.on_link_event(LinkEvent::PromptStopped("pipe".into()), window, cx);
                assert!(!panel.review.turn_open(), "the turn ended with the prompt");
                panel.revert_turn(1, cx);
                assert_eq!(
                    panel.notice().map(str::to_string).as_deref(),
                    Some(
                        "turn 1 reverted: 1 hunk(s) put back, 0 STALE kept, \
                         0 changed again by a later turn, kept, 0 accepted, kept"
                    )
                );
            })
            .unwrap();
        assert_eq!(buffer.read_with(cx, |b, _| b.text()), ORIGINAL);
        let (f, _buffer) = fixture(cx).await;
        f.window
            .update(cx, |panel, window, cx| {
                panel.turn = Turn::Streaming;
                let taken = LinkEvent::Taken {
                    session_id: "s".into(),
                    reason: "another driver".into(),
                };
                panel.on_link_event(taken, window, cx);
                assert!(!panel.review.turn_open(), "a taken session ends the turn");
            })
            .unwrap();
    }

    /// `(path, key)` of the first hunk of the first reviewed file.
    fn first_hunk(f: &Fixture, cx: &mut TestAppContext) -> (PathBuf, HunkKey) {
        f.window
            .update(cx, |panel, _, cx| {
                let file = &panel.review.files()[0];
                (panel.review.path(file, cx), file.hunks()[0].key.clone())
            })
            .unwrap()
    }

    fn notice(f: &Fixture, cx: &mut TestAppContext) -> Option<String> {
        f.window
            .update(cx, |panel, _, _| panel.notice().map(str::to_string))
            .unwrap()
    }

    fn state_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cedian-panel-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// ADR-0032: each correction the person makes in Review Changes is one
    /// row in the ledger, however often the review rebuilds.
    #[gpui::test]
    async fn review_corrections_are_ledger_rows(cx: &mut TestAppContext) {
        use cedian_shell::corrections::{self, CorrectionKind};
        let (f, buffer) = fixture(cx).await;
        let dir = state_dir("corrections");
        f.window
            .update(cx, |panel, _, _| panel.set_state_dir(dir.clone()))
            .unwrap();
        tool_start(&f, cx, "c1", &["notes.txt"]);
        omp_writes(&f, "/ws/notes.txt", "ALPHA\nbeta\nGAMMA\n").await;
        tool_end(&f, cx, "c1");
        let mut vcx = VisualTestContext::from_window(f.window.into(), cx);
        click(&mut vcx, "cedian-review-toggle");
        click(&mut vcx, "cedian-reject-ws/notes.txt-0");
        buffer.update(&mut vcx, |b, cx| {
            let at = b.text().find("GAMMA").unwrap();
            b.edit([(at..at + 5, "Gamma by hand")], None, cx);
        });
        for _ in 0..3 {
            f.window
                .update(&mut vcx, |panel, _, cx| panel.toggle_review(cx))
                .unwrap();
            vcx.run_until_parked();
        }
        settled(&f, &mut vcx);
        f.window
            .update(&mut vcx, |panel, _, cx| panel.revert_turn(1, cx))
            .unwrap();
        let rows = corrections::load(&dir).unwrap();
        let kinds: Vec<CorrectionKind> = rows.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                CorrectionKind::HunkRejected,
                CorrectionKind::UserEditedAgentHunk,
                CorrectionKind::TurnReverted
            ]
        );
        let rejected = &rows[0];
        assert_eq!(rejected.path.as_deref(), Some("ws/notes.txt"));
        assert_eq!(rejected.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(rejected.turn, Some(1));
        assert!(rejected.hunk_key.is_some() && rejected.excerpt_hash.is_some());
        assert_eq!(rows[1].path.as_deref(), Some("ws/notes.txt"));
        assert_eq!(rows[2].turn, Some(1));
        assert!(rows.iter().all(|r| r.task == "panel"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// While a later call is writing the file, a keystroke inside a Pending
    /// hunk is not yet classified, so Reject must refuse rather than write
    /// over it (ADR-0006). Once the call ends the hunk is STALE and Reject
    /// is refused as such.
    #[gpui::test]
    async fn reject_is_refused_while_the_file_is_importing(cx: &mut TestAppContext) {
        let (f, buffer) = fixture(cx).await;
        tool_start(&f, cx, "c1", &["notes.txt"]);
        omp_writes(&f, "/ws/notes.txt", "alpha\nBETA\ngamma\n").await;
        tool_end(&f, cx, "c1");
        assert_eq!(hunks(&f, cx)[0].1, HunkStatus::Pending);
        tool_start(&f, cx, "c2", &["notes.txt"]);
        buffer.update(cx, |b, cx| b.edit([(7..7, "!")], None, cx));
        cx.run_until_parked();
        // The keystroke is not classified yet, so the view still shows the
        // hunk as Pending with its Reject button: that click is the one
        // that must be refused.
        assert_eq!(hunks(&f, cx)[0].1, HunkStatus::Pending);
        let (path, key) = first_hunk(&f, cx);
        f.window
            .update(cx, |panel, _, cx| panel.reject_hunk(&path, &key, cx))
            .unwrap();
        assert_eq!(
            notice(&f, cx).as_deref(),
            Some("OMP is writing ws/notes.txt; try again when the call ends")
        );
        assert_eq!(
            buffer.read_with(cx, |b, _| b.text()),
            "alpha\nB!ETA\ngamma\n",
            "the keystroke is kept"
        );
        let refused = Some("OMP is writing ws/notes.txt; try again when the call ends");
        f.window
            .update(cx, |panel, _, cx| {
                panel.notice = None;
                panel.accept_all(cx);
                assert_eq!(panel.notice(), refused, "Accept all");
                panel.notice = None;
                panel.review.end_turn();
                panel.revert_turn(1, cx);
                assert_eq!(panel.notice(), refused, "Revert turn");
            })
            .unwrap();
        assert_eq!(
            hunks(&f, cx)[0].1,
            HunkStatus::Pending,
            "nothing was accepted"
        );
        tool_end(&f, cx, "c2");
        assert_eq!(hunks(&f, cx)[0].1, HunkStatus::Stale);
        let (path, key) = first_hunk(&f, cx);
        f.window
            .update(cx, |panel, _, cx| panel.reject_hunk(&path, &key, cx))
            .unwrap();
        assert!(
            notice(&f, cx).is_some_and(|n| n.contains("hunk is STALE")),
            "{:?}",
            notice(&f, cx)
        );
        assert_eq!(
            buffer.read_with(cx, |b, _| b.text()),
            "alpha\nB!ETA\ngamma\n"
        );
    }

    /// The session is taken while a call is in flight: its ToolEnd never
    /// comes, so the call is released and the file is the person's again.
    #[gpui::test]
    async fn a_taken_session_releases_the_calls_in_flight(cx: &mut TestAppContext) {
        let (f, buffer) = fixture(cx).await;
        tool_start(&f, cx, "c1", &["notes.txt"]);
        omp_writes(&f, "/ws/notes.txt", "alpha\nBETA\ngamma\n").await;
        tool_end(&f, cx, "c1");
        tool_start(&f, cx, "c2", &["notes.txt"]);
        f.window
            .update(cx, |panel, window, cx| {
                panel.turn = Turn::Streaming;
                let taken = LinkEvent::Taken {
                    session_id: "s".into(),
                    reason: "another driver".into(),
                };
                panel.on_link_event(taken, window, cx);
                assert!(
                    panel.review.files().iter().all(|f| !f.importing()),
                    "no file is importing"
                );
            })
            .unwrap();
        buffer.update(cx, |b, cx| b.edit([(7..7, "!")], None, cx));
        cx.run_until_parked();
        assert_eq!(hunks(&f, cx)[0].1, HunkStatus::Stale);
    }

    /// Two calls write the same file at once and the short one ends first:
    /// the file is still importing for the long one, so its write is not
    /// a user edit when it lands.
    #[gpui::test]
    async fn overlapping_calls_keep_the_file_importing_until_the_last_ends(
        cx: &mut TestAppContext,
    ) {
        let (f, _buffer) = fixture(cx).await;
        tool_start(&f, cx, "c1", &["notes.txt"]);
        omp_writes(&f, "/ws/notes.txt", "alpha\nBETA\ngamma\n").await;
        tool_end(&f, cx, "c1");
        tool_start(&f, cx, "c2", &["notes.txt"]);
        tool_start(&f, cx, "c3", &["notes.txt"]);
        tool_end(&f, cx, "c3");
        omp_writes(&f, "/ws/notes.txt", "alpha\nBETA\nGAMMA\n").await;
        tool_end(&f, cx, "c2");
        assert_eq!(
            hunks(&f, cx),
            vec![
                (
                    "ws/notes.txt".to_string(),
                    HunkStatus::Pending,
                    vec!["c1".to_string()]
                ),
                (
                    "ws/notes.txt".to_string(),
                    HunkStatus::Pending,
                    vec!["c2".to_string()]
                ),
            ]
        );
    }

    /// OMP's process exits mid-turn: the turn is over, so its revert is not
    /// refused as still running.
    #[gpui::test]
    async fn a_disconnect_mid_turn_ends_the_reviews_turn(cx: &mut TestAppContext) {
        let (f, buffer) = fixture(cx).await;
        tool_start(&f, cx, "c1", &["notes.txt"]);
        omp_writes(&f, "/ws/notes.txt", "alpha\nBETA\ngamma\n").await;
        tool_end(&f, cx, "c1");
        f.window
            .update(cx, |panel, window, cx| {
                panel.turn = Turn::Streaming;
                panel.on_link_event(LinkEvent::Event(RouterEvent::Disconnected), window, cx);
                assert!(!panel.review.turn_open(), "the turn ended with the process");
                panel.revert_turn(1, cx);
                assert_eq!(
                    panel.notice().map(str::to_string).as_deref(),
                    Some(
                        "turn 1 reverted: 1 hunk(s) put back, 0 STALE kept, \
                         0 changed again by a later turn, kept, 0 accepted, kept"
                    )
                );
            })
            .unwrap();
        assert_eq!(buffer.read_with(cx, |b, _| b.text()), ORIGINAL);
    }

    /// Typing in an open file OMP did not write is no outcome: the file is
    /// not in review at all, STALE or otherwise.
    #[gpui::test]
    async fn an_unrelated_dirty_file_is_not_in_review(cx: &mut TestAppContext) {
        let (f, _notes) = fixture(cx).await;
        let other = f
            .project
            .update(cx, |p, cx| p.open_local_buffer("/ws/other.txt", cx))
            .await
            .unwrap();
        tool_start(&f, cx, "c1", &["notes.txt"]);
        other.update(cx, |b, cx| b.edit([(0..0, "USER ")], None, cx));
        omp_writes(&f, "/ws/notes.txt", "alpha\nBETA\ngamma\n").await;
        tool_end(&f, cx, "c1");
        let files: Vec<(String, Option<String>)> = f
            .window
            .update(cx, |panel, _, cx| {
                panel
                    .review
                    .files()
                    .iter()
                    .map(|file| {
                        (
                            panel.review.path(file, cx).display().to_string(),
                            file.stale_import().map(str::to_string),
                        )
                    })
                    .collect()
            })
            .unwrap();
        assert_eq!(files, vec![("ws/notes.txt".to_string(), None)]);
        assert_eq!(hunks(&f, cx)[0].1, HunkStatus::Pending);
    }

    /// The person saves their own edit to an open file while a call writes
    /// another: a file the call does not name is never an outcome, so their
    /// save is not credited to the agent and the other file's hunk stands.
    #[gpui::test]
    async fn a_users_save_of_an_unnamed_file_is_not_imported(cx: &mut TestAppContext) {
        let (f, _notes) = fixture(cx).await;
        let other = f
            .project
            .update(cx, |p, cx| p.open_local_buffer("/ws/other.txt", cx))
            .await
            .unwrap();
        tool_start(&f, cx, "c1", &["notes.txt"]);
        other.update(cx, |b, cx| b.edit([(0..0, "USER ")], None, cx));
        f.project
            .update(cx, |p, cx| p.save_buffer(other.clone(), cx))
            .await
            .unwrap();
        omp_writes(&f, "/ws/notes.txt", "alpha\nBETA\ngamma\n").await;
        tool_end(&f, cx, "c1");
        assert_eq!(
            hunks(&f, cx),
            vec![(
                "ws/notes.txt".to_string(),
                HunkStatus::Pending,
                vec!["c1".to_string()]
            )]
        );
        assert_eq!(other.read_with(cx, |b, _| b.text()), "USER one\ntwo\n");
    }

    /// A file already dirty before the call, named by the call but left as
    /// it was on disk: the disk did not change, so it is no outcome. It is
    /// not listed STALE and the person's text is kept.
    #[gpui::test]
    async fn a_named_file_dirty_before_the_call_whose_disk_is_unchanged_is_no_outcome(
        cx: &mut TestAppContext,
    ) {
        let (f, _notes) = fixture(cx).await;
        let other = f
            .project
            .update(cx, |p, cx| p.open_local_buffer("/ws/other.txt", cx))
            .await
            .unwrap();
        other.update(cx, |b, cx| b.edit([(0..0, "USER ")], None, cx));
        tool_start(&f, cx, "c1", &["notes.txt", "other.txt"]);
        cx.run_until_parked();
        omp_writes(&f, "/ws/notes.txt", "alpha\nBETA\ngamma\n").await;
        tool_end(&f, cx, "c1");
        let files: Vec<(String, Option<String>)> = f
            .window
            .update(cx, |panel, _, cx| {
                panel
                    .review
                    .files()
                    .iter()
                    .map(|file| {
                        (
                            panel.review.path(file, cx).display().to_string(),
                            file.stale_import().map(str::to_string),
                        )
                    })
                    .collect()
            })
            .unwrap();
        assert_eq!(files, vec![("ws/notes.txt".to_string(), None)]);
        other.read_with(cx, |b, _| {
            assert_eq!(b.text(), "USER one\ntwo\n");
            assert!(b.is_dirty());
        });
    }

    /// Two calls start on a file with no review yet and the short one ends
    /// first: its outcome creates the file's review, which must know the
    /// long call is still writing it, so Reject is refused until that one
    /// ends.
    #[gpui::test]
    async fn a_review_created_mid_call_knows_the_call_still_writing_it(cx: &mut TestAppContext) {
        let (f, buffer) = fixture(cx).await;
        tool_start(&f, cx, "c1", &["notes.txt"]);
        tool_start(&f, cx, "c2", &["notes.txt"]);
        omp_writes(&f, "/ws/notes.txt", "alpha\nBETA\ngamma\n").await;
        tool_end(&f, cx, "c2");
        assert_eq!(hunks(&f, cx)[0].1, HunkStatus::Pending);
        let (path, key) = first_hunk(&f, cx);
        f.window
            .update(cx, |panel, _, cx| panel.reject_hunk(&path, &key, cx))
            .unwrap();
        assert_eq!(
            notice(&f, cx).as_deref(),
            Some("OMP is writing ws/notes.txt; try again when the call ends")
        );
        assert_eq!(
            buffer.read_with(cx, |b, _| b.text()),
            "alpha\nBETA\ngamma\n",
            "nothing was put back"
        );
        tool_end(&f, cx, "c1");
        f.window
            .update(cx, |panel, _, cx| {
                panel.notice = None;
                panel.reject_hunk(&path, &key, cx);
                assert_eq!(panel.notice(), None, "the reject went through");
            })
            .unwrap();
        assert_eq!(buffer.read_with(cx, |b, _| b.text()), ORIGINAL);
    }

    #[gpui::test]
    #[ignore]
    async fn live_panel_streams_and_imports_one_undoable_omp_edit(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init(cx);
        let dir = std::env::temp_dir().join(format!("cedian-s9a-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let dir = dir.canonicalize().unwrap();

        let fs = fs::RealFs::new(None, cx.executor());
        let project = Project::test(fs, [dir.as_path()], cx).await;
        let buffer = project
            .update(cx, |p, cx| p.open_local_buffer(dir.join("notes.txt"), cx))
            .await
            .unwrap();
        let window = cx.add_window(|window, cx| CedianPanel::new(project.clone(), window, cx));

        let send = |cx: &mut TestAppContext, text: &'static str| {
            window
                .update(cx, |panel, window, cx| {
                    panel
                        .input
                        .update(cx, |editor, cx| editor.set_text(text, window, cx));
                    panel.send(window, cx);
                    assert_eq!(panel.turn, Turn::Queued, "{:?}", panel.notice);
                })
                .unwrap();
        };
        let read = |cx: &mut TestAppContext| {
            window
                .update(cx, |panel, _, _| {
                    (
                        panel.turn.clone(),
                        texts(panel),
                        panel
                            .review
                            .files()
                            .iter()
                            .map(|f| f.agent_txns().len())
                            .sum::<usize>(),
                        panel
                            .review
                            .files()
                            .iter()
                            .filter(|f| f.stale_import().is_some())
                            .count(),
                    )
                })
                .unwrap()
        };

        // T2: a prompt typed into the panel streams a reply into the thread.
        send(
            cx,
            "Reply with exactly this word and nothing else: hello-cedian",
        );
        wait_until(cx, "streamed reply", |cx| {
            let (status, text, _, _) = read(cx);
            status == Turn::Idle && text.contains("hello-cedian")
        });
        eprintln!("--- after T2 ---\n{}", read(cx).1);

        // T5: OMP's own edit tool writes disk → one imported transaction.
        send(
            cx,
            "Use your edit tool (not bash, not a host tool) to change the line `beta` to `BETA` \
             in notes.txt in the current workspace. Then reply with only: done",
        );
        wait_until(cx, "imported agent edit", |cx| {
            let (status, _, imported, _) = read(cx);
            status == Turn::Idle && imported >= 1
        });
        let (_, text, imported, stale) = read(cx);
        eprintln!("--- after T5 ---\n{text}\nimported={imported} stale={stale}");
        assert_eq!(imported, 1, "exactly one agent transaction");
        assert_eq!(stale, 0);
        let call_id = window
            .update(cx, |panel, _, _| {
                panel.review.files()[0].agent_txns()[0].tool_call_id.clone()
            })
            .unwrap();
        assert!(!call_id.is_empty(), "keyed by tool_call_id");

        buffer.update(cx, |b, cx| {
            assert_eq!(b.text(), "alpha\nBETA\ngamma\n", "buffer shows OMP's write");
            assert!(!b.is_dirty());
            b.undo(cx);
            assert_eq!(
                b.text(),
                "alpha\nbeta\ngamma\n",
                "ONE native undo reverts it"
            );
        });
        eprintln!("tool_call_id={call_id}: OMP edit imported and reverted by one undo");
    }
}

/// A tool card, or a subagent shown under the card that started it.
enum Entry<'a> {
    Card(ToolCard),
    Subagent(&'a SubagentRow),
}

fn card_line(card: &ToolCard) -> String {
    format!("[{:?}] {}", card.status, card.display_line())
}

fn subagent_line(row: &SubagentRow) -> String {
    format!("↳ [{:?}] {} — {}", row.status, row.agent, row.description)
}

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

use crate::dialogs::OpenDialog;
use crate::import::{self, ImportOutcome, Mark};
use crate::omp_link::{AnswerError, LaunchSpec, LinkEvent, OmpLink, Prompt};
use crate::omp_settings::OmpSettings;
use cedian_agent::Thread;
use cedian_omp::{RouterEvent, UserAnswer};
use collections::{HashMap, IndexMap};
use editor::{Editor, actions::Paste};
use futures::{StreamExt, channel::mpsc};
use gpui::{
    Action, AnyElement, App, AsyncWindowContext, ClipboardEntry, Context, ElementId, Entity,
    EventEmitter, FocusHandle, Focusable, Pixels, Render, Task, WeakEntity, Window, actions, px,
};
use language::Buffer;
use omp_rpc::{ExtensionUiRequest, ImageContent};
use project::Project;
use std::path::PathBuf;
use std::time::Duration;
use text::TransactionId;
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

/// One agent edit imported as a native transaction.
pub struct ImportedEdit {
    pub tool_call_id: String,
    pub buffer: Entity<Buffer>,
    pub transaction: TransactionId,
}

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

pub struct CedianPanel {
    focus_handle: FocusHandle,
    project: Entity<Project>,
    input: Entity<Editor>,
    thread: Thread,
    link: Option<OmpLink>,
    connection: Connection,
    marks: HashMap<String, Vec<(Entity<Buffer>, Mark)>>,
    imported: Vec<ImportedEdit>,
    stale: usize,
    turn: Turn,
    /// The last thing the person should know that is not on a dialog: a
    /// refused prompt, a failed turn, a dialog cedian closed.
    notice: Option<String>,
    /// OMP's dialogs waiting on the person, by request id, oldest first.
    dialogs: IndexMap<String, OpenDialog>,
    /// Images pasted into the composer, sent with the next prompt.
    images: Vec<ImageContent>,
    /// The OMP settings page, shown instead of the thread when open.
    settings: Option<Entity<OmpSettings>>,
    show_settings: bool,
    _events: Option<Task<()>>,
}

impl CedianPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            workspace.update_in(cx, |workspace, window, cx| {
                let project = workspace.project().clone();
                cx.new(|cx| Self::new(project, window, cx))
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
            link: None,
            connection: Connection::NotStarted,
            marks: HashMap::default(),
            imported: Vec::new(),
            stale: 0,
            turn: Turn::Idle,
            notice: None,
            dialogs: IndexMap::default(),
            images: Vec::new(),
            settings: None,
            show_settings: false,
            _events: None,
        };
        if this.workspace_root(cx).is_some() {
            this.start(window, cx);
        }
        this
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

    /// Agent edits imported so far (newest last).
    pub fn imported(&self) -> &[ImportedEdit] {
        &self.imported
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
        self.link = None;
        self.dialogs.clear();
        let Some(root) = self.workspace_root(cx) else {
            self.connection = Connection::Stopped("open a folder first".to_string());
            return;
        };
        let spec = match LaunchSpec::resolve(&root) {
            Ok(spec) => spec,
            Err(e) => {
                self.connection = Connection::Stopped(e);
                cx.notify();
                return;
            }
        };
        let (event_tx, mut event_rx) = mpsc::unbounded::<LinkEvent>();
        self.link = Some(OmpLink::start(spec, event_tx));
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

    /// The Retry button of a taken session.
    pub fn retry_session(&mut self, cx: &mut Context<Self>) {
        if let Some(Err(e)) = self.link.as_ref().map(OmpLink::retry) {
            self.notice = Some(e);
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
                self.connection = Connection::Ready {
                    session_id,
                    resumed,
                    policy_note,
                };
            }
            LinkEvent::Failed(reason) => self.stop(reason),
            LinkEvent::Taken { session_id, reason } => {
                self.turn = Turn::Idle;
                self.connection = Connection::Taken { session_id, reason };
            }
            LinkEvent::AuditFailed(e) => self.audit_failed(e, cx),
            LinkEvent::Event(RouterEvent::UiRequest(ExtensionUiRequest::Cancel(cancel))) => {
                self.dialogs.shift_remove(&cancel.target_id);
            }
            LinkEvent::Event(RouterEvent::UiRequest(request)) => {
                let id = cedian_omp::dialog::dialog(&request).map(|(id, _)| id.to_string());
                if let Some(id) = id
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
                self.stop("OMP stopped: its process exited".to_string());
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

    fn stop(&mut self, reason: String) {
        self.link = None;
        self.dialogs.clear();
        self.marks.clear();
        self.turn = Turn::Idle;
        self.connection = Connection::Stopped(reason);
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text(cx);
        if text.trim().is_empty() {
            return;
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
                text: text.clone(),
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
        self.notice = None;
        self.turn = Turn::Queued;
        cx.notify();
    }

    fn on_event(&mut self, event: RouterEvent, cx: &mut Context<Self>) {
        match &event {
            RouterEvent::ToolStart {
                tool_call_id,
                tool_name,
                ..
            } if EDIT_TOOLS.contains(&tool_name.as_str()) => {
                let buffers: Vec<_> = self
                    .project
                    .read(cx)
                    .opened_buffers(cx)
                    .into_iter()
                    .filter(|b| b.read(cx).file().is_some_and(|f| f.is_local()))
                    .collect();
                let marks = buffers
                    .into_iter()
                    .map(|b| {
                        let mark = b.update(cx, |b, _| import::begin(b));
                        (b, mark)
                    })
                    .collect();
                self.marks.insert(tool_call_id.clone(), marks);
            }
            RouterEvent::ToolEnd {
                tool_call_id,
                is_error,
                ..
            } => {
                if let Some(marks) = self.marks.remove(tool_call_id) {
                    if !is_error {
                        self.import(tool_call_id.clone(), marks, cx);
                    }
                }
            }
            RouterEvent::AgentStart if self.turn == Turn::Queued => self.turn = Turn::Streaming,
            RouterEvent::Settled => {
                if let Turn::Failed(reason) = std::mem::replace(&mut self.turn, Turn::Idle) {
                    self.notice = Some(format!("turn failed: {reason}"));
                }
            }
            _ => {}
        }
        self.thread.apply(&event);
        cx.notify();
    }

    /// The Stop button: close the dialogs OMP waits on and abort the
    /// running turn. The turn goes idle when OMP reports the session settled.
    pub fn stop_turn(&mut self, cx: &mut Context<Self>) {
        self.turn = Turn::Stopping;
        let closed = self.link.as_ref().map(OmpLink::close_dialogs);
        self.dialogs.clear();
        match closed {
            Some(Err(e)) => self.audit_failed(e, cx),
            _ => self.abort(cx),
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
                self.turn = Turn::Failed(reason);
                self.abort(cx);
            }
        }
        cx.notify();
    }

    fn abort(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.connection, Connection::Ready { .. }) {
            return;
        }
        let Some(control) = self.link.as_ref().and_then(OmpLink::control) else {
            return;
        };
        let aborted = cx.background_spawn(async move { control.abort() });
        cx.spawn(async move |this, cx| {
            if let Err(e) = aborted.await {
                this.update(cx, |this, cx| {
                    if this.turn != Turn::Idle {
                        this.turn = Turn::Failed(format!("stop: {e}"));
                    }
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
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
                self.notice = Some(format!(
                    "\"{}\" got no answer in {} minutes; cedian dismissed it",
                    dialog.title().lines().next().unwrap_or_default(),
                    DIALOG_TIMEOUT.as_secs() / 60
                ))
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

    fn import(
        &mut self,
        tool_call_id: String,
        marks: Vec<(Entity<Buffer>, Mark)>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            for (buffer, mark) in marks {
                let outcome = import::finish(buffer.clone(), mark, cx).await;
                this.update(cx, |this, cx| {
                    match outcome {
                        Ok(ImportOutcome::Imported(transaction)) => {
                            this.imported.push(ImportedEdit {
                                tool_call_id: tool_call_id.clone(),
                                buffer,
                                transaction,
                            })
                        }
                        Ok(ImportOutcome::Stale) => this.stale += 1,
                        Ok(ImportOutcome::Unchanged) => {}
                        Err(e) => log::error!("cedian: import failed: {e}"),
                    }
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }
}

impl EventEmitter<PanelEvent> for CedianPanel {}

impl Focusable for CedianPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CedianPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (messages, cards) = cedian_agent_ui::render_thread(self.thread.events());
        let rows = messages
            .into_iter()
            .map(|m| Label::new(format!("{:?}: {}", m.role, m.text)).into_any_element())
            .chain(cards.into_iter().map(|c| {
                Label::new(format!("[{:?}] {}", c.status, c.display_line()))
                    .color(Color::Muted)
                    .into_any_element()
            }));
        let connection = match &self.connection {
            Connection::NotStarted => "OMP not started".to_string(),
            Connection::Starting => "starting OMP…".to_string(),
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
        let stoppable = matches!(self.turn, Turn::Queued | Turn::Streaming | Turn::Failed(_));
        let images = self.images.len();
        let footer = format!(
            "{} · {} · imported {} agent edit(s){}",
            connection,
            self.turn.label(),
            self.imported.len(),
            if self.stale > 0 {
                format!(" · {} STALE", self.stale)
            } else {
                String::new()
            }
        );
        v_flex()
            .key_context("CedianPanel")
            .track_focus(&self.focus_handle)
            .capture_action(cx.listener(Self::paste))
            .size_full()
            .p_2()
            .gap_2()
            .child(
                h_flex().justify_end().child(
                    Button::new(
                        "cedian-settings",
                        if self.show_settings {
                            "Back to chat"
                        } else {
                            "OMP settings"
                        },
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_settings(window, cx))),
                ),
            )
            .when_some(
                self.settings.clone().filter(|_| self.show_settings),
                |panel, settings| panel.child(div().flex_1().child(settings)),
            )
            .when(!self.show_settings, |panel| {
                panel.child(
                    v_flex()
                        .id("cedian-thread")
                        .flex_1()
                        .overflow_y_scroll()
                        .gap_1()
                        .children(rows),
                )
            })
            .children(dialogs)
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
                            .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
                    )
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
    use gpui::TestAppContext;
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
                        panel.imported.len(),
                        panel.stale,
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
            .update(cx, |panel, _, _| panel.imported[0].tool_call_id.clone())
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

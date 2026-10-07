//! The cedian dock panel (S9a T2/T4, S9 U3): one prompt box, the streamed OMP
//! reply rendered from the headless `cedian_agent::Thread`, and agent-edit
//! import.
//!
//! The panel starts OMP when it opens on a folder ([`crate::omp_link`]): its
//! own thread, the user's `cedian.toml`, the spawn profile. Events cross into
//! GPUI over a channel; edit-class tool events drive [`crate::import`]. When
//! OMP dies the panel says why and offers Restart; the IDE keeps running.

use crate::import::{self, ImportOutcome, Mark};
use crate::omp_link::{LaunchSpec, LinkEvent, OmpLink};
use crate::omp_settings::OmpSettings;
use cedian_agent::Thread;
use cedian_omp::RouterEvent;
use collections::HashMap;
use editor::Editor;
use futures::{StreamExt, channel::mpsc};
use gpui::{
    Action, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels,
    Render, Task, WeakEntity, Window, actions, px,
};
use language::Buffer;
use project::Project;
use std::path::PathBuf;
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
    status: String,
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
            status: "idle".to_string(),
            settings: None,
            show_settings: false,
            _events: None,
        };
        if this.workspace_root(cx).is_some() {
            this.start(cx);
        }
        this
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// The turn status line: `idle`, `streaming`, or `error: …`.
    pub fn status(&self) -> &str {
        &self.status
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
    fn start(&mut self, cx: &mut Context<Self>) {
        self.link = None;
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
        self._events = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = event_rx.next().await {
                if this
                    .update(cx, |this, cx| this.on_link_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// The Restart button: a fresh OMP on the same session.
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        self.start(cx);
    }

    fn on_link_event(&mut self, event: LinkEvent, cx: &mut Context<Self>) {
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
        self.marks.clear();
        self.status = "idle".to_string();
        self.connection = Connection::Stopped(reason);
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text(cx);
        if text.trim().is_empty() {
            return;
        }
        if self.connection == Connection::NotStarted {
            self.start(cx);
        }
        let sent = match (&self.connection, &self.link) {
            (Connection::Stopped(reason), _) => Err(format!("{reason}; restart OMP")),
            (_, Some(link)) => link.send(text.clone()),
            (_, None) => Err("OMP is not running; restart it".to_string()),
        };
        if let Err(e) = sent {
            self.status = format!("error: {e}");
            cx.notify();
            return;
        }
        self.input.update(cx, |editor, cx| editor.clear(window, cx));
        self.thread.push_user(&text);
        self.status = "streaming".to_string();
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
            RouterEvent::Settled => self.status = "idle".to_string(),
            _ => {}
        }
        self.thread.apply(&event);
        cx.notify();
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
        };
        let stopped = matches!(self.connection, Connection::Stopped(_));
        let footer = format!(
            "{} · {} · imported {} agent edit(s){}",
            connection,
            self.status,
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
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(self.input.clone()))
                    .child(
                        Button::new("cedian-send", "Send")
                            .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new(footer).size(LabelSize::Small).color(if stopped {
                        Color::Error
                    } else {
                        Color::Muted
                    }))
                    .when(stopped, |row| {
                        row.child(
                            Button::new("cedian-restart", "Restart OMP")
                                .on_click(cx.listener(|this, _, _, cx| this.restart(cx))),
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
                    assert!(!panel.status.starts_with("error"), "{}", panel.status);
                })
                .unwrap();
        };
        let read = |cx: &mut TestAppContext| {
            window
                .update(cx, |panel, _, _| {
                    (
                        panel.status.clone(),
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
            status == "idle" && text.contains("hello-cedian")
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
            status == "idle" && imported >= 1
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

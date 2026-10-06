//! The cedian dock panel (S9a T2/T4): one prompt box, the streamed OMP reply
//! rendered from the headless `cedian_agent::Thread`, and agent-edit import.
//!
//! OMP runs on its own OS thread (the runtime is blocking, never on the UI
//! thread) and is spawned through the P1 spawn profile. Router events cross
//! into GPUI over a channel; edit-class tool events drive [`crate::import`].

use crate::import::{self, ImportOutcome, Mark};
use cedian_agent::Thread;
use cedian_omp::{OmpBinary, OmpRuntime, RouterEvent, RuntimeConfig, SpawnPolicy};
use collections::HashMap;
use editor::Editor;
use futures::{StreamExt, channel::mpsc};
use gpui::{
    Action, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Pixels, Render, Task, WeakEntity, Window, actions, px,
};
use language::Buffer;
use project::Project;
use std::{path::PathBuf, time::Duration};
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

pub struct CedianPanel {
    focus_handle: FocusHandle,
    project: Entity<Project>,
    input: Entity<Editor>,
    thread: Thread,
    prompts: Option<std::sync::mpsc::Sender<String>>,
    marks: HashMap<String, Vec<(Entity<Buffer>, Mark)>>,
    imported: Vec<ImportedEdit>,
    stale: usize,
    status: String,
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

    fn new(project: Entity<Project>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Ask OMP…", window, cx);
            editor
        });
        Self {
            focus_handle: cx.focus_handle(),
            project,
            input,
            thread: Thread::new(),
            prompts: None,
            marks: HashMap::default(),
            imported: Vec::new(),
            stale: 0,
            status: "idle".to_string(),
            _events: None,
        }
    }

    /// Agent edits imported so far (newest last).
    pub fn imported(&self) -> &[ImportedEdit] {
        &self.imported
    }

    fn workspace_root(&self, cx: &App) -> Option<PathBuf> {
        let worktree = self.project.read(cx).visible_worktrees(cx).next()?;
        Some(worktree.read(cx).abs_path().to_path_buf())
    }

    /// Start OMP on first use: runtime thread + event bridge.
    fn ensure_runtime(&mut self, cx: &mut Context<Self>) -> anyhow::Result<()> {
        if self.prompts.is_some() {
            return Ok(());
        }
        let cwd = self
            .workspace_root(cx)
            .ok_or_else(|| anyhow::anyhow!("open a folder first"))?;
        let binary = match std::env::var("CEDIAN_OMP_BINARY") {
            Ok(path) => OmpBinary::Bundled(PathBuf::from(path)),
            Err(_) => OmpBinary::Path("omp".to_string()),
        };
        let config = RuntimeConfig {
            binary,
            session_dir: std::env::temp_dir().join("cedian-app-sessions"),
            cwd,
            ask_dialog: true,
            prompt_timeout: Duration::from_secs(600),
            policy: SpawnPolicy::default(),
        };
        let (prompt_tx, prompt_rx) = std::sync::mpsc::channel::<String>();
        let (event_tx, mut event_rx) = mpsc::unbounded::<RouterEvent>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

        std::thread::Builder::new()
            .name("cedian-omp".to_string())
            .spawn(move || {
                let mut runtime = match OmpRuntime::spawn(config) {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                let router = runtime.router();
                let (_sub, events) = router.subscribe();
                std::thread::spawn(move || {
                    for event in events {
                        if event_tx.unbounded_send(event).is_err() {
                            break;
                        }
                    }
                });
                let _ = ready_tx.send(Ok(()));
                for prompt in prompt_rx {
                    if let Err(e) = runtime.prompt(&prompt, vec![]) {
                        log::error!("cedian: OMP turn failed: {e}");
                    }
                }
                let _ = runtime.shutdown();
            })?;
        ready_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("OMP thread died"))?
            .map_err(|e| anyhow::anyhow!(e))?;

        self._events = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = event_rx.next().await {
                if this
                    .update(cx, |this, cx| this.on_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
        self.prompts = Some(prompt_tx);
        Ok(())
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text(cx);
        if text.trim().is_empty() {
            return;
        }
        if let Err(e) = self.ensure_runtime(cx) {
            self.status = format!("error: {e}");
            cx.notify();
            return;
        }
        self.input.update(cx, |editor, cx| editor.clear(window, cx));
        self.thread.push_user(&text);
        if let Some(prompts) = &self.prompts {
            let _ = prompts.send(text);
        }
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
        let footer = format!(
            "{} · imported {} agent edit(s){}",
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
                v_flex()
                    .id("cedian-thread")
                    .flex_1()
                    .overflow_y_scroll()
                    .gap_1()
                    .children(rows),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(self.input.clone()))
                    .child(
                        Button::new("cedian-send", "Send")
                            .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
                    ),
            )
            .child(Label::new(footer).size(LabelSize::Small).color(Color::Muted))
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

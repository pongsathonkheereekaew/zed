//! The OMP settings page (S9 U3a, ADR-0040, ADR-0045): every OMP setting
//! with its effective value and the layer that supplies it, read and written
//! through OMP's own CLI. Live: it re-reads when OMP's global or project
//! `config.yml` changes on disk, and on `config_update`.

use crate::omp_link::LaunchSpec;
use cedian_omp::omp_config::{Entry, Setting, layered, overlay_keys};
use cedian_omp::{Layer, OmpConfig};
use collections::HashSet;
use editor::Editor;
use futures::StreamExt as _;
use gpui::{App, Context, Entity, FocusHandle, Focusable, Render, Task, Window};
use project::Project;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use ui::{Button, Label, prelude::*};

/// Rows drawn at once; the filter narrows the rest.
const MAX_ROWS: usize = 200;

pub struct OmpSettings {
    focus_handle: FocusHandle,
    project: Entity<Project>,
    workdir: PathBuf,
    config: Option<OmpConfig>,
    pinned: Arc<BTreeSet<String>>,
    defaults: Option<Arc<BTreeMap<String, Entry>>>,
    settings: Vec<Setting>,
    message: Option<String>,
    reloads: usize,
    filter: Entity<Editor>,
    value: Entity<Editor>,
    selected: Option<String>,
    _watch: Option<Task<()>>,
}

impl OmpSettings {
    pub fn new(
        project: Entity<Project>,
        workdir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter OMP settings…", window, cx);
            editor
        });
        cx.subscribe(&filter, |_, _, event: &editor::EditorEvent, cx| {
            if matches!(event, editor::EditorEvent::BufferEdited) {
                cx.notify();
            }
        })
        .detach();
        let value = cx.new(|cx| Editor::single_line(window, cx));
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            project,
            workdir,
            config: None,
            pinned: Arc::default(),
            defaults: None,
            settings: Vec::new(),
            message: None,
            reloads: 0,
            filter,
            value,
            selected: None,
            _watch: None,
        };
        match LaunchSpec::resolve(&this.workdir).and_then(|spec| {
            let pinned = spec.policy.overlay().map_err(|e| e.to_string())?;
            Ok((OmpConfig::new(spec.binary), overlay_keys(&pinned)))
        }) {
            Ok((config, pinned)) => {
                this.config = Some(config);
                this.pinned = Arc::new(pinned);
                this.watch(cx);
                this.reload(cx);
            }
            Err(e) => this.message = Some(e),
        }
        this
    }

    pub fn settings(&self) -> &[Setting] {
        &self.settings
    }

    pub fn setting(&self, key: &str) -> Option<&Setting> {
        self.settings.iter().find(|s| s.key == key)
    }

    /// The last write's outcome or the last error.
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// Completed reads, for watching the page refresh.
    pub fn reloads(&self) -> usize {
        self.reloads
    }

    /// Re-read every value and its layer through OMP.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(config) = self.config.clone() else {
            return;
        };
        let workdir = self.workdir.clone();
        let pinned = Arc::clone(&self.pinned);
        let defaults = self.defaults.clone();
        let scratch = match scratch_dirs(&workdir) {
            Ok(dirs) => dirs,
            Err(e) => {
                self.message = Some(e);
                return;
            }
        };
        let read = cx.background_spawn(async move {
            let defaults = match defaults {
                Some(d) => d,
                None => Arc::new(config.defaults(&scratch.1).map_err(|e| e.to_string())?),
            };
            let effective = config.list(&workdir).map_err(|e| e.to_string())?;
            let global = config.list(&scratch.0).map_err(|e| e.to_string())?;
            Ok::<_, String>((layered(effective, &global, &defaults, &pinned), defaults))
        });
        cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok((settings, defaults)) => {
                        this.settings = settings;
                        this.defaults = Some(defaults);
                    }
                    Err(e) => this.message = Some(format!("cannot read OMP's settings: {e}")),
                }
                this.reloads += 1;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Write `key` through `omp config set` (the global file).
    pub fn set_value(&mut self, key: &str, value: &str, cx: &mut Context<Self>) {
        let (key, value) = (key.to_string(), value.to_string());
        self.write(cx, move |config, cwd| config.set(cwd, &key, &value));
    }

    pub fn reset(&mut self, key: &str, cx: &mut Context<Self>) {
        let key = key.to_string();
        self.write(cx, move |config, cwd| config.reset(cwd, &key));
    }

    /// Set one `modelRoles` entry: OMP takes the record whole, so read it,
    /// change the entry, write it back (ADR-0045 decision 3).
    pub fn set_role(&mut self, role: &str, model: &str, cx: &mut Context<Self>) {
        let mut roles = match self
            .setting("modelRoles")
            .and_then(|s| s.entry.value.clone())
        {
            Some(Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };
        roles.insert(role.to_string(), Value::String(model.to_string()));
        self.set_value("modelRoles", &Value::Object(roles).to_string(), cx);
    }

    fn write(
        &mut self,
        cx: &mut Context<Self>,
        op: impl FnOnce(&OmpConfig, &Path) -> Result<cedian_omp::WriteOutcome, cedian_omp::OmpError>
        + Send
        + 'static,
    ) {
        let Some(config) = self.config.clone() else {
            return;
        };
        let workdir = self.workdir.clone();
        let task = cx.background_spawn(async move { op(&config, &workdir) });
        cx.spawn(async move |this, cx| {
            let outcome = task.await;
            this.update(cx, |this, cx| {
                this.message = Some(match outcome {
                    Ok(out) => match out.overridden_by {
                        Some(layer) => format!(
                            "saved to OMP's global config, but the {layer} layer still wins (now {})",
                            out.value
                        ),
                        None => format!("saved: {}", out.value),
                    },
                    Err(cedian_omp::OmpError::Command { error, .. }) => {
                        format!("OMP refused: {error}")
                    }
                    Err(e) => format!("cannot reach OMP's config: {e}"),
                });
                this.reload(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Watch OMP's global and project `config.yml` (ADR-0045 decision 1).
    fn watch(&mut self, cx: &mut Context<Self>) {
        let Some(config) = self.config.clone() else {
            return;
        };
        let fs = self.project.read(cx).fs().clone();
        let workdir = self.workdir.clone();
        self._watch = Some(cx.spawn(async move |this, cx| {
            let agent = cx
                .background_spawn({
                    let workdir = workdir.clone();
                    async move { config.dir(&workdir) }
                })
                .await;
            let mut targets: HashSet<PathBuf> = HashSet::default();
            targets.insert(workdir.join(".omp/config.yml"));
            let mut dirs = vec![workdir.clone()];
            if let Ok(agent) = agent {
                targets.insert(agent.join("config.yml"));
                dirs.push(agent);
            }
            let mut streams = Vec::new();
            let mut watchers = Vec::new();
            for dir in dirs {
                let (stream, watcher) = fs.watch(&dir, Duration::from_millis(100)).await;
                streams.push(stream);
                watchers.push(watcher);
            }
            let mut events = futures::stream::select_all(streams);
            while let Some(batch) = events.next().await {
                if batch.iter().any(|e| targets.contains(&e.path)) {
                    if this.update(cx, |this, cx| this.reload(cx)).is_err() {
                        break;
                    }
                }
            }
            drop(watchers);
        }));
    }

    fn select(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(key.to_string());
        let text = self
            .setting(key)
            .and_then(|s| s.entry.value.as_ref())
            .map(display_value)
            .unwrap_or_default();
        self.value
            .update(cx, |editor, cx| editor.set_text(text, window, cx));
        cx.notify();
    }

    fn apply_input(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.selected.clone() else {
            return;
        };
        let text = self.value.read(cx).text(cx);
        match key.strip_prefix("modelRoles.") {
            Some(role) => self.set_role(role, text.trim(), cx),
            None => self.set_value(&key, text.trim(), cx),
        }
    }
}

/// An empty dir (global + defaults) and an empty agent dir (defaults only).
fn scratch_dirs(workdir: &Path) -> Result<(PathBuf, PathBuf), String> {
    let base = cedian_shell::state::dir(workdir)?.join("omp-settings");
    let dirs = (base.join("empty"), base.join("defaults"));
    for dir in [&dirs.0, &dirs.1] {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    Ok(dirs)
}

fn display_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn layer_color(layer: Layer) -> Color {
    match layer {
        Layer::Default => Color::Muted,
        Layer::Global => Color::Default,
        Layer::Project => Color::Accent,
        Layer::Cedian => Color::Warning,
    }
}

impl Focusable for OmpSettings {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for OmpSettings {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.filter.read(cx).text(cx).to_lowercase();
        let roles: Vec<(String, String)> = match self
            .setting("modelRoles")
            .and_then(|s| s.entry.value.as_ref())
        {
            Some(Value::Object(map)) => map
                .iter()
                .map(|(k, v)| (k.clone(), display_value(v)))
                .collect(),
            _ => Vec::new(),
        };
        let role_layer = self.setting("modelRoles").map(|s| s.layer.label());
        let rows = self
            .settings
            .iter()
            .filter(|s| query.is_empty() || s.key.to_lowercase().contains(&query))
            .take(MAX_ROWS)
            .map(|s| {
                let key = s.key.clone();
                let value = s
                    .entry
                    .value
                    .as_ref()
                    .map(display_value)
                    .unwrap_or_else(|| "(hidden)".to_string());
                h_flex()
                    .id(SharedString::from(format!("omp-setting-{key}")))
                    .gap_2()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| this.select(&key, window, cx)))
                    .child(Label::new(s.key.clone()).size(LabelSize::Small))
                    .child(Label::new(value).size(LabelSize::Small).color(Color::Muted))
                    .child(
                        Label::new(s.layer.label())
                            .size(LabelSize::XSmall)
                            .color(layer_color(s.layer)),
                    )
                    .into_any_element()
            });
        let editing = self.selected.clone().map(|key| {
            let editable =
                key.starts_with("modelRoles.") || self.setting(&key).is_some_and(Setting::editable);
            let reset_key = key.clone();
            h_flex()
                .gap_2()
                .child(Label::new(key).size(LabelSize::Small))
                .when(editable, |row| {
                    row.child(div().flex_1().child(self.value.clone()))
                        .child(
                            Button::new("omp-setting-set", "Set")
                                .on_click(cx.listener(|this, _, _, cx| this.apply_input(cx))),
                        )
                        .child(Button::new("omp-setting-reset", "Reset").on_click(
                            cx.listener(move |this, _, _, cx| this.reset(&reset_key, cx)),
                        ))
                })
                .when(!editable, |row| {
                    row.child(
                        Label::new("records and lists are read-only here; edit them in OMP")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                })
        });
        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .gap_2()
            .child(
                Label::new("OMP settings · read and written through OMP · layers are derived")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .children(
                self.message
                    .clone()
                    .map(|m| Label::new(m).size(LabelSize::Small)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(Label::new(format!(
                        "Model roles{}",
                        role_layer.map(|l| format!(" ({l})")).unwrap_or_default()
                    )))
                    .children(roles.into_iter().map(|(role, model)| {
                        let key = format!("modelRoles.{role}");
                        let shown = format!("{role}: {model}");
                        h_flex()
                            .id(SharedString::from(format!("omp-role-{role}")))
                            .gap_2()
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.selected = Some(key.clone());
                                this.value.update(cx, |editor, cx| {
                                    editor.set_text(model.clone(), window, cx)
                                });
                                cx.notify();
                            }))
                            .child(Label::new(shown).size(LabelSize::Small))
                            .into_any_element()
                    })),
            )
            .child(self.filter.clone())
            .children(editing)
            .child(
                v_flex()
                    .id("omp-settings-rows")
                    .flex_1()
                    .overflow_y_scroll()
                    .gap_0p5()
                    .children(rows),
            )
    }
}

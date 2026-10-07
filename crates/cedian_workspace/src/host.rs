//! `WorkspaceHost`: the editor boundary (plan §12 trait shape).
//!
//! Headless implementation over [`BufferStore`]: selection/active-file are
//! caller-provided (no editor yet — the Zed binding feeds real cursor state),
//! `apply_edit` is the transaction path OMP host tools call, Agent Sync
//! (§13) saves dirty buffers before the turn.
//!
//! Threading: the store lives behind `parking_lot::Mutex`; host-tool handler
//! threads (vendored client) lock briefly per call — no await, no UI thread.

use crate::buffer::{ApplyEditResult, BufferError, BufferStore, TextEdit, Version};
use omp_rpc::{HostTool, HostUri};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Name of the host edit tool (also allow-listed in the OMP spawn overlay).
pub const APPLY_EDIT_TOOL: &str = "cedian_apply_edit";
/// Normalize any key-shaped path to canonical `/rel` form (leading `/`
/// enforced, `.` components kept — keys never touch disk by themselves).
fn normalize_key(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    let stripped = s.strip_prefix('/').unwrap_or(&s);
    PathBuf::from(format!("/{stripped}"))
}

/// One published diagnostic (headless subset of LSP `Diagnostic`; the fork
/// binding converts real `lsp::Diagnostic` into this shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Buffer path.
    pub path: PathBuf,
    /// 0-based line.
    pub line: usize,
    pub severity: DiagnosticSeverity,
    pub message: String,
}

/// Diagnostic severity (LSP subset).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
}

/// Editor boundary: active file, selection, versioned edits, save, diagnostics.
/// Mirrors plan §12 (`buffer_version` + `apply_edit(path, expected, edit)`);
/// the Zed binding implements this same trait over `clock::Global` + `History`.
pub trait WorkspaceHost: Send + Sync {
    /// File the user is editing, if any.
    fn active_file(&self) -> Option<PathBuf>;
    /// Current selection as `(path, start, end)` byte offsets, if any.
    fn selection(&self) -> Option<(PathBuf, usize, usize)>;
    /// Current buffer version (optimistic-concurrency token).
    fn buffer_version(&self, path: &Path) -> Option<Version>;
    /// Apply one edit transactionally. Version mismatch fails closed.
    fn apply_edit(
        &self,
        path: &Path,
        expected: Version,
        edit: &TextEdit,
    ) -> Result<ApplyEditResult, BufferError>;
    /// Undo the newest edit on one buffer (no-op when clean).
    fn undo(&self, path: &Path) -> Option<Version>;
    /// Read buffer text (agents see buffers, not just the filesystem — §13).
    fn read_buffer(&self, path: &Path) -> Option<String>;
    /// Published diagnostics for one buffer (fork: real LSP state).
    fn diagnostics(&self, path: &Path) -> Vec<Diagnostic>;
}

/// Headless host: in-memory buffers + caller-set editor chrome + published
/// diagnostics (headless stand-in for Zed `Project::diagnostics`; the fork
/// binding publishes real LSP diagnostics here).
///
/// Path discipline: buffers are keyed by workspace-RELATIVE keys (`/src/a.rs`);
/// `workdir` anchors them to disk. Host-tool/URI paths resolve through
/// [`HostTools::resolve`] — absolute paths under workdir, `/`-prefixed keys,
/// and bare relative paths all land on the same buffer; paths escaping the
/// workspace are rejected (fail closed, never a jailbreak).
pub struct HostTools {
    store: Mutex<BufferStore>,
    active_file: Mutex<Option<PathBuf>>,
    selection: Mutex<Option<(PathBuf, usize, usize)>>,
    diagnostics: Mutex<HashMap<PathBuf, Vec<Diagnostic>>>,
    workdir: PathBuf,
    /// Live LSP bridge (S1): `None` until `attach_lsp` runs.
    lsp: Mutex<Option<Arc<crate::LspBridge>>>,
}

impl HostTools {
    /// Empty host rooted at `workdir` (disk anchor for resolve + auto-open).
    /// Canonicalizes (macOS `/tmp` → `/private/tmp`): without this, server
    /// URIs never match `resolve` and diagnostics silently vanish.
    pub fn new(workdir: &Path) -> Self {
        Self {
            store: Mutex::new(BufferStore::new()),
            active_file: Mutex::new(None),
            selection: Mutex::new(None),
            diagnostics: Mutex::new(HashMap::new()),
            workdir: workdir
                .canonicalize()
                .unwrap_or_else(|_| workdir.to_path_buf()),
            lsp: Mutex::new(None),
        }
    }

    /// Shared handle (host-tool/URI handlers capture this).
    pub fn shared(workdir: &Path) -> Arc<Self> {
        Arc::new(Self::new(workdir))
    }

    /// Open buffer keys (for CLI sync + `cedian://open-editors`).
    pub fn open_keys(&self) -> Vec<PathBuf> {
        self.store.lock().open_paths()
    }

    /// Open a buffer with initial text (test setup / workspace scan). Accepts
    /// canonical keys directly — no re-resolve (a `/`-key is absolute-shaped
    /// but is NOT outside workdir; resolving it again would wrongly reject).
    /// Untrusted spellings go through [`HostTools::resolve`] first.
    pub fn open(&self, path: &Path, text: &str) {
        let key = normalize_key(path);
        self.store.lock().open(&key, text);
    }

    /// Re-read a buffer from disk text; see [`BufferStore::reload`].
    pub fn reload(&self, path: &Path, text: &str) -> bool {
        let key = normalize_key(path);
        self.store.lock().reload(&key, text)
    }

    /// Resolve any workspace path spelling to its canonical buffer key:
    /// absolute-under-workdir, `/`-prefixed key, or bare relative. Rejects
    /// escapes (`..` past root, absolute outside workdir) with a visible error.
    pub fn resolve(&self, path: &Path) -> Result<PathBuf, String> {
        // Normalize BEFORE prefix-strip: server URIs may use uncanonical
        // spellings (`/tmp/...` vs `/private/tmp/...`) while workdir is
        // canonical. `canonicalize` needs existence — lexical `/tmp`
        // fallback needs none.
        let canon = path.canonicalize().unwrap_or_else(|_| {
            let s = path.to_string_lossy();
            if s.starts_with("/tmp/") {
                PathBuf::from(s.replacen("/tmp/", "/private/tmp/", 1))
            } else {
                path.to_path_buf()
            }
        });
        let rel = if canon.is_absolute() {
            match canon.strip_prefix(&self.workdir) {
                Ok(r) => r.to_path_buf(),
                // A `/`-key (`/notes.txt`) is absolute-shaped too. Accept it
                // only when it names an open buffer or a file inside the
                // workspace, so it can never address anything outside.
                Err(_) if self.is_workspace_key(path) => {
                    path.strip_prefix("/").unwrap_or(path).to_path_buf()
                }
                Err(_) => return Err(format!("path escapes workspace: {}", path.display())),
            }
        } else {
            path.strip_prefix("/").unwrap_or(path).to_path_buf()
        };
        if rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!("path escapes workspace: {}", path.display()));
        }
        Ok(PathBuf::from(format!("/{}", rel.display())))
    }
    /// Whether `/`-prefixed `path` names an open buffer or a workspace file.
    fn is_workspace_key(&self, path: &Path) -> bool {
        let key = normalize_key(path);
        if self.store.lock().version(&key).is_some() {
            return true;
        }
        let rel = path.strip_prefix("/").unwrap_or(path);
        !rel.components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
            && self.workdir.join(rel).is_file()
    }

    /// Ensure a buffer is open for a key, loading from disk on first use.
    /// Returns `false` when there is nothing to open (no disk file either) —
    /// callers fail with a visible "not open" error, never an empty buffer.
    fn ensure_open(&self, key: &Path) -> bool {
        if self.store.lock().version(key).is_some() {
            return true;
        }
        let rel = key.strip_prefix("/").unwrap_or(key);
        let local = self.workdir.join(rel);
        match std::fs::read_to_string(&local) {
            Ok(text) => {
                self.store.lock().open(key, &text);
                true
            }
            Err(_) => false,
        }
    }

    /// Set the active file (Zed binding: cursor moves).
    pub fn set_active_file(&self, path: Option<PathBuf>) {
        *self.active_file.lock() = path;
    }

    /// Set the selection (Zed binding: selection changes).
    pub fn set_selection(&self, sel: Option<(PathBuf, usize, usize)>) {
        *self.selection.lock() = sel;
    }

    /// Agent Sync (§13): save every dirty buffer, return saved paths.
    /// Idempotent — a crash between save and prompt loses nothing (saves are
    /// plain version stamps; re-running saves the same content).
    pub fn agent_sync(&self) -> Vec<PathBuf> {
        let mut store = self.store.lock();
        let dirty = store.dirty_buffers();
        for path in &dirty {
            store.mark_saved(path);
        }
        dirty
    }

    /// Build the `cedian_apply_edit` host tool: the OMP-side route calls this
    /// for cedian-relevant paths (plan §10: host tools, never `cedian_edit`).
    /// Args: `{path, expected_version, start, end, replacement}`.
    pub fn apply_edit_tool(self: &Arc<Self>) -> HostTool {
        let host = Arc::clone(self);
        let params: Map<String, Value> = serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "expected_version": {"type": "integer"},
                "start": {"type": "integer"},
                "end": {"type": "integer"},
                "replacement": {"type": "string"}
            },
            "required": ["path", "expected_version", "start", "end", "replacement"],
            "additionalProperties": false
        })
        .as_object()
        .unwrap()
        .clone();
        HostTool::new(
            APPLY_EDIT_TOOL,
            "Apply one edit to a cedian workspace buffer transactionally (preferred over filesystem writes for project files).",
            params,
            move |args, _ctx| {
                let raw = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let key = match host.resolve(Path::new(raw)) {
                    Ok(key) => key,
                    Err(e) => return Err(e.into()),
                };
                if !host.ensure_open(&key) {
                    return Err(format!("buffer not open (no file on disk): {raw}").into());
                }
                let edit = TextEdit {
                    start: args.get("start").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
                    end: args.get("end").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
                    replacement: args
                        .get("replacement")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                };
                let expected = Version(
                    args.get("expected_version")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                );
                match host.apply_edit(&key, expected, &edit) {
                    Ok(result) => {
                        Ok(format!("applied, now at version {}", result.new_version.0).into())
                    }
                    Err(e) => Err(e.to_string().into()),
                }
            },
        )
    }

    /// Build the `cedian` URI scheme: serves every Phase 6 kind (buffer,
    /// selection, active-file, diagnostics, open-editors). Unknown kinds fail
    /// with `isError`, never silent empty (forward-compat for browser/ios/review).
    pub fn cedian_uri_scheme(self: &Arc<Self>) -> HostUri {
        let host = Arc::clone(self);
        HostUri::new("cedian", move |url, _ctx| {
            let uri = crate::parse_cedian_uri(url)
                .map_err(|e| -> omp_rpc::HostUriError { e.to_string().into() })?;
            host.serve_uri(&uri)
        })
        .expect("cedian scheme is valid")
    }

    /// Serve one parsed `cedian://` URL. Pure dispatch — each kind has its own
    /// renderer below so Phase 7+ (browser/ios/review) extends by adding arms.
    fn serve_uri(
        &self,
        uri: &crate::CedianUri,
    ) -> Result<omp_rpc::HostUriRead, omp_rpc::HostUriError> {
        use crate::UriKind;
        match &uri.kind {
            UriKind::Buffer => {
                let key = self
                    .resolve(Path::new(&uri.path))
                    .map_err(|e| -> omp_rpc::HostUriError { e.into() })?;
                if !self.ensure_open(&key) {
                    return Err(format!("buffer not open: {}", uri.path).into());
                }
                self.read_buffer(&key).map(|text| text.into()).ok_or_else(
                    || -> omp_rpc::HostUriError { format!("buffer not open: {}", uri.path).into() },
                )
            }
            UriKind::Selection => Ok(self.render_selection().into()),
            UriKind::ActiveFile => Ok(self.render_active_file().into()),
            UriKind::Diagnostics => Ok(self.render_diagnostics(&uri.path).into()),
            UriKind::OpenEditors => Ok(self.render_open_editors().into()),
            UriKind::Symbols => self.serve_symbols(&uri.path),
            UriKind::Unknown(kind) => Err(format!("unknown cedian:// kind: {kind}").into()),
        }
    }
    /// Attach a live LSP bridge (S1). Replaces any previous bridge.
    /// Also syncs every currently-open buffer into the server.
    pub fn attach_lsp(&self, bridge: Arc<crate::LspBridge>) {
        // Snapshot under one short lock: a guard in a `for` head lives for the
        // whole loop, so re-locking inside it would deadlock (parking_lot is
        // not reentrant).
        let open: Vec<(PathBuf, String)> = {
            let store = self.store.lock();
            store
                .open_paths()
                .into_iter()
                .filter_map(|key| store.read(&key).map(|(text, _)| (key, text)))
                .collect()
        };
        for (key, text) in open {
            let local = self.workdir.join(key.strip_prefix("/").unwrap_or(&key));
            bridge.sync_buffer(&local, &text);
        }
        *self.lsp.lock() = Some(bridge);
    }

    /// Serve `cedian://symbols/...`: `file/<path>` → document symbols,
    /// anything else → workspace search. No bridge → visible error (never
    /// silent empty — the caller must attach LSP first).
    fn serve_symbols(&self, path: &str) -> Result<omp_rpc::HostUriRead, omp_rpc::HostUriError> {
        let bridge = self
            .lsp
            .lock()
            .clone()
            .ok_or_else(|| -> omp_rpc::HostUriError {
                "no LSP bridge attached (attach_lsp first)"
                    .to_string()
                    .into()
            })?;
        if let Some(file) = path.strip_prefix("file/") {
            let key = self
                .resolve(Path::new(file))
                .map_err(|e| -> omp_rpc::HostUriError { e.into() })?;
            let local = self.workdir.join(key.strip_prefix("/").unwrap_or(&key));
            let symbols = bridge
                .document_symbols(&local)
                .map_err(|e| -> omp_rpc::HostUriError { e.to_string().into() })?;
            return Ok(crate::lsp_bridge::render_workspace_symbols(&symbols).into());
        }
        let symbols = bridge
            .workspace_symbols(path)
            .map_err(|e| -> omp_rpc::HostUriError { e.to_string().into() })?;
        Ok(crate::lsp_bridge::render_workspace_symbols(&symbols).into())
    }

    /// Publish diagnostics for a buffer (fork: LSP publishDiagnostics lands here).
    pub fn publish_diagnostics(&self, path: &Path, diagnostics: Vec<Diagnostic>) {
        self.diagnostics
            .lock()
            .insert(path.to_path_buf(), diagnostics);
    }

    /// Render `cedian://selection`: `path:start-end` + selected text, or empty
    /// (no silent stale — empty means no selection NOW).
    fn render_selection(&self) -> String {
        match self.selection() {
            Some((path, start, end)) => {
                let text = self.read_buffer(&path).unwrap_or_default();
                let end = end.min(text.len());
                let start = start.min(end);
                format!(
                    "{}:{}-{}\n{}",
                    path.display(),
                    start,
                    end,
                    &text[start..end]
                )
            }
            None => String::new(),
        }
    }

    /// Render `cedian://active-file`: the active path, or empty.
    fn render_active_file(&self) -> String {
        self.active_file()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Render `cedian://diagnostics[/path]`: `severity path:line message` lines.
    /// Empty path = all buffers. Empty output = no diagnostics (not an error).
    /// Path matches with or without a leading `/` (URI paths strip it).
    fn render_diagnostics(&self, path: &str) -> String {
        let all = self.diagnostics.lock();
        let mut lines = Vec::new();
        for (buf, diags) in all.iter() {
            if !path.is_empty() {
                let buf_str = buf.to_string_lossy();
                let buf_no_slash = buf_str.strip_prefix('/').unwrap_or(&buf_str);
                if buf_no_slash != path && buf_str != path {
                    continue;
                }
            }
            for d in diags {
                let sev = match d.severity {
                    DiagnosticSeverity::Error => "error",
                    DiagnosticSeverity::Warning => "warning",
                    DiagnosticSeverity::Info => "info",
                };
                lines.push(format!(
                    "{sev} {}:{} {}",
                    d.path.display(),
                    d.line,
                    d.message
                ));
            }
        }
        lines.sort();
        lines.join("\n")
    }

    /// Render `cedian://open-editors`: one open buffer path per line.
    fn render_open_editors(&self) -> String {
        let mut paths: Vec<String> = self
            .store
            .lock()
            .open_paths()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        paths.sort();
        paths.join("\n")
    }
}

impl WorkspaceHost for HostTools {
    fn active_file(&self) -> Option<PathBuf> {
        self.active_file.lock().clone()
    }

    fn selection(&self) -> Option<(PathBuf, usize, usize)> {
        self.selection.lock().clone()
    }

    fn buffer_version(&self, path: &Path) -> Option<Version> {
        self.store.lock().version(path)
    }

    fn apply_edit(
        &self,
        path: &Path,
        expected: Version,
        edit: &TextEdit,
    ) -> Result<ApplyEditResult, BufferError> {
        self.store.lock().apply_edit(path, expected, edit)
    }

    fn undo(&self, path: &Path) -> Option<Version> {
        self.store.lock().undo(path)
    }

    fn read_buffer(&self, path: &Path) -> Option<String> {
        self.store.lock().read(path).map(|(text, _)| text)
    }

    fn diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        self.diagnostics
            .lock()
            .get(path)
            .cloned()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_saves_dirty() {
        let h = HostTools::new(Path::new("/"));
        h.open(Path::new("/a.rs"), "hi");
        h.apply_edit(
            Path::new("/a.rs"),
            Version(0),
            &TextEdit {
                start: 0,
                end: 2,
                replacement: "yo".into(),
            },
        )
        .unwrap();
        let saved = h.agent_sync();
        assert_eq!(saved, vec![PathBuf::from("/a.rs")]);
        assert!(h.agent_sync().is_empty());
    }
    #[test]
    fn uri_kinds_serve() {
        use crate::{CedianUri, UriKind};
        let h = HostTools::new(Path::new("/"));
        h.open(Path::new("/a.rs"), "hello\nworld\n");
        h.open(Path::new("/b.rs"), "x\n");
        h.set_active_file(Some(PathBuf::from("/a.rs")));
        h.set_selection(Some((PathBuf::from("/a.rs"), 0, 5)));
        h.publish_diagnostics(
            Path::new("/a.rs"),
            vec![Diagnostic {
                path: PathBuf::from("/a.rs"),
                line: 1,
                severity: DiagnosticSeverity::Warning,
                message: "unused".to_string(),
            }],
        );
        let serve = |kind: UriKind, path: &str| {
            h.serve_uri(&CedianUri {
                kind,
                path: path.to_string(),
            })
            .unwrap()
            .content
        };
        assert_eq!(serve(UriKind::Buffer, "/a.rs"), "hello\nworld\n");
        assert!(serve(UriKind::Selection, "").contains("/a.rs:0-5"));
        assert_eq!(serve(UriKind::ActiveFile, ""), "/a.rs");
        assert!(serve(UriKind::Diagnostics, "").contains("warning /a.rs:1 unused"));
        assert!(serve(UriKind::Diagnostics, "a.rs").contains("unused"));
        assert!(!serve(UriKind::Diagnostics, "b.rs").contains("unused"));
        let editors = serve(UriKind::OpenEditors, "");
        assert!(editors.contains("/a.rs") && editors.contains("/b.rs"));
        // Unknown kind: visible error, never silent empty.
        assert!(
            h.serve_uri(&CedianUri {
                kind: UriKind::Unknown("nope".into()),
                path: String::new()
            })
            .is_err()
        );
        // Missing buffer: visible error.
        assert!(
            h.serve_uri(&CedianUri {
                kind: UriKind::Buffer,
                path: "/zzz.rs".into()
            })
            .is_err()
        );
    }

    #[test]
    fn tmp_symlink_resolves() {
        // macOS `/tmp` → `/private/tmp`: server URIs use either spelling.
        std::fs::create_dir_all("/tmp/fake-ws-resolve").unwrap();
        let h = HostTools::new(Path::new("/tmp/fake-ws-resolve"));
        let key = h
            .resolve(Path::new("/tmp/fake-ws-resolve/fake.rs"))
            .expect("resolves");
        assert_eq!(key, PathBuf::from("/fake.rs"));
    }

    #[test]
    fn slash_key_resolves_only_inside_workspace() {
        let ws = std::env::temp_dir().join(format!("cedian-ws-slashkey-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("notes.txt"), "x\n").unwrap();
        let ws = ws.canonicalize().unwrap();
        let h = HostTools::new(&ws);
        // `/`-key naming a workspace file (what models copy from buffer keys).
        assert_eq!(
            h.resolve(Path::new("/notes.txt")).unwrap(),
            PathBuf::from("/notes.txt")
        );
        // `/`-key naming an open (unsaved) buffer.
        h.open(Path::new("/draft.rs"), "fn main() {}\n");
        assert_eq!(
            h.resolve(Path::new("/draft.rs")).unwrap(),
            PathBuf::from("/draft.rs")
        );
        // Real absolute paths outside the workspace stay rejected.
        assert!(h.resolve(Path::new("/etc/hosts")).is_err());
        assert!(h.resolve(Path::new("/nope.txt")).is_err());
        assert!(h.resolve(Path::new("/../etc/hosts")).is_err());
        let _ = std::fs::remove_dir_all(&ws);
    }
}

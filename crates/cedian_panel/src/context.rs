//! What the person sees, for OMP (ARCHITECTURE §39, §40): a bounded snapshot
//! goes in front of each prompt, and the full detail is served from Zed
//! through `cedian://` reads OMP makes.
//!
//! Both read the same thing: a [`HostTools`] filled from the open buffers of
//! the workspace folder, the active editor's selection, and the diagnostics
//! Zed's language servers published on those buffers. Buffers outside the
//! folder and Zed's private files (`private_files`, e.g. `.env`) never enter
//! it.

use cedian_workspace::{Diagnostic, DiagnosticSeverity, HostTools};
use editor::Editor;
use futures::channel::mpsc::UnboundedSender;
use gpui::{App, Entity, WeakEntity};
use language::{Buffer, Point};
use omp_rpc::{HostUri, HostUriRead};
use project::Project;
use std::path::PathBuf;
use std::time::Duration;
use workspace::Workspace;

/// How long a `cedian://` read waits for the app.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// One `cedian://` read OMP asked for, answered on the GPUI thread.
pub(crate) struct Read {
    pub url: String,
    pub reply: std::sync::mpsc::Sender<Result<HostUriRead, String>>,
}

/// The `cedian` scheme: each read crosses to the GPUI thread over `reads`.
pub(crate) fn scheme(reads: UnboundedSender<Read>) -> HostUri {
    HostUri::new("cedian", move |url, _ctx| {
        let (reply, answer) = std::sync::mpsc::channel();
        reads
            .unbounded_send(Read {
                url: url.to_string(),
                reply,
            })
            .map_err(|_| "the cedian panel is closed")?;
        match answer.recv_timeout(READ_TIMEOUT) {
            Ok(read) => read.map_err(Into::into),
            Err(_) => Err(format!("no answer from the app for {url}").into()),
        }
    })
    .expect("cedian scheme is valid")
}

/// The open buffers, selection and diagnostics of `project`'s folder.
pub(crate) fn host(
    workspace: Option<&WeakEntity<Workspace>>,
    project: &Entity<Project>,
    cx: &App,
) -> Option<HostTools> {
    let worktree = project.read(cx).visible_worktrees(cx).next()?;
    let root_id = worktree.read(cx).id();
    let host = HostTools::new(&worktree.read(cx).abs_path());
    let key = |buffer: &Entity<Buffer>| -> Option<PathBuf> {
        let file = project::File::from_dyn(buffer.read(cx).file())?;
        (file.worktree_id(cx) == root_id && !file.is_private)
            .then(|| PathBuf::from(format!("/{}", file.path.as_unix_str())))
    };
    for buffer in project.read(cx).opened_buffers(cx) {
        let Some(path) = key(&buffer) else { continue };
        let snapshot = buffer.read(cx).snapshot();
        host.open(&path, &snapshot.text());
        let diagnostics = snapshot
            .diagnostics_in_range::<_, Point>(0..snapshot.len(), false)
            .filter(|entry| entry.diagnostic.is_primary)
            .map(|entry| Diagnostic {
                path: path.clone(),
                line: entry.range.start.row as usize,
                severity: match entry.diagnostic.severity {
                    language::DiagnosticSeverity::ERROR => DiagnosticSeverity::Error,
                    language::DiagnosticSeverity::WARNING => DiagnosticSeverity::Warning,
                    _ => DiagnosticSeverity::Info,
                },
                message: entry.diagnostic.message.to_string(),
            })
            .collect::<Vec<_>>();
        if !diagnostics.is_empty() {
            host.publish_diagnostics(&path, diagnostics);
        }
    }
    let editor = workspace
        .and_then(|w| w.upgrade())
        .and_then(|w| w.read(cx).active_item_as::<Editor>(cx));
    if let Some(editor) = editor {
        let editor = editor.read(cx);
        let multi = editor.buffer().read(cx);
        if let Some(path) = multi.as_singleton().and_then(|b| key(&b)) {
            host.set_active_file(Some(path.clone()));
            let snapshot = multi.snapshot(cx);
            let selection = editor.selections.newest_anchor();
            let start = snapshot.point_to_buffer_offset(selection.start);
            let end = snapshot.point_to_buffer_offset(selection.end);
            if let (Some((_, start)), Some((_, end))) = (start, end)
                && start != end
            {
                host.set_selection(Some((path, start.0, end.0)));
            }
        }
    }
    Some(host)
}

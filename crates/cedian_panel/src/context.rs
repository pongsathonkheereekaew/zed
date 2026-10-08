//! What the person sees, for OMP (ARCHITECTURE §39, §40): a bounded snapshot
//! goes in front of each prompt, and the full detail is served from Zed
//! through `cedian://` reads OMP makes.
//!
//! Both read the same thing: a [`HostTools`] filled from the open buffers of
//! the workspace folder, the active editor's selection, and the diagnostics
//! Zed's language servers published on those buffers. Buffers outside the
//! folder and Zed's private files (`private_files`, e.g. `.env`) never enter
//! it.

use cedian_workspace::{CedianUri, Diagnostic, DiagnosticSeverity, HostTools, UriKind};
use editor::Editor;
use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use gpui::{App, AppContext as _, Entity, Task, WeakEntity};
use language::{Buffer, Location, Point, PointUtf16, ToPoint as _};
use omp_rpc::{HostUri, HostUriRead};
use project::{Project, WorktreeId};
use std::path::{Path, PathBuf};
use std::time::Duration;
use util::paths::PathStyle;
use util::rel_path::RelPath;
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

/// Answer `cedian://` reads, one at a time, with `answer`, called when
/// each arrives; `None` (the panel is gone) stops.
pub(crate) fn serve(
    mut reads: UnboundedReceiver<Read>,
    answer: impl Fn(&str, &mut App) -> Option<Task<Result<HostUriRead, String>>> + 'static,
    cx: &mut App,
) -> Task<()> {
    cx.spawn(async move |cx| {
        while let Some(read) = reads.next().await {
            let Some(answer) = cx.update(|cx| answer(&read.url, cx)) else {
                break;
            };
            let _ = read.reply.send(answer.await);
        }
    })
}

/// Answer one `cedian://` read from Zed.
pub(crate) fn answer(
    url: &str,
    workspace: Option<&WeakEntity<Workspace>>,
    project: &Entity<Project>,
    cx: &mut App,
) -> Task<Result<HostUriRead, String>> {
    let Some(host) = host(workspace, project, cx) else {
        return Task::ready(Err("no folder is open".to_string()));
    };
    match cedian_workspace::parse_cedian_uri(url) {
        Ok(
            uri @ CedianUri {
                kind: UriKind::Definitions | UriKind::References | UriKind::Symbols,
                ..
            },
        ) => lsp_read(&host, project, &uri, cx),
        Ok(CedianUri {
            kind: UriKind::Diagnostics,
            path,
        }) => Task::ready(Ok(host.render_diagnostics(&path).into())),
        Ok(CedianUri {
            kind: UriKind::Buffer,
            path,
        }) => buffer_read(&host, project, &path, cx),
        _ => Task::ready(host.read_uri(url).map_err(|e| e.to_string())),
    }
}

/// The text Zed holds for `file`, unsaved edits included.
fn buffer_read(
    host: &HostTools,
    project: &Entity<Project>,
    file: &str,
    cx: &mut App,
) -> Task<Result<HostUriRead, String>> {
    let path = inside(host, project, file, cx);
    let project = project.clone();
    cx.spawn(async move |cx| {
        let path = path.await?;
        let buffer = project
            .update(cx, |p, cx| p.open_local_buffer(&path, cx))
            .await
            .map_err(|e| e.to_string())?;
        Ok(cx.update(|cx| buffer.read(cx).text()).into())
    })
}

/// The absolute path of `file` in the folder. Refused when it is private
/// (`private_files`) or resolves, symlinks followed, outside the folder.
fn inside(
    host: &HostTools,
    project: &Entity<Project>,
    file: &str,
    cx: &mut App,
) -> Task<Result<PathBuf, String>> {
    let Some(tree) = project.read(cx).visible_worktrees(cx).next() else {
        return Task::ready(Err("no folder is open".to_string()));
    };
    let key = match host.resolve(Path::new(file)) {
        Ok(key) => key,
        Err(e) => return Task::ready(Err(e)),
    };
    let root = tree.read(cx).abs_path().to_path_buf();
    let path = root.join(key.strip_prefix("/").unwrap_or(&key));
    let fs = project.read(cx).fs().clone();
    let file = file.to_string();
    cx.spawn(async move |cx| {
        let private = |rel: &Path, cx: &mut gpui::AsyncApp| {
            cx.update(|cx| {
                let Ok(rel) = RelPath::new(rel, PathStyle::local()) else {
                    return true;
                };
                let tree = tree.read(cx);
                tree.entry_for_path(&rel).is_some_and(|e| e.is_private)
                    || tree.as_local().is_some_and(|t| t.is_path_private(&rel))
            })
        };
        if private(path.strip_prefix(&root).unwrap_or(&path), cx) {
            return Err(format!("{file} is private (private_files)"));
        }
        let canonical = fs
            .canonicalize(&path)
            .await
            .map_err(|_| format!("no such file in the folder: {file}"))?;
        let canonical_root = fs.canonicalize(&root).await.map_err(|e| e.to_string())?;
        let Ok(rel) = canonical.strip_prefix(&canonical_root) else {
            return Err(format!("{file} resolves outside the folder"));
        };
        if private(rel, cx) {
            return Err(format!("{file} is private (private_files)"));
        }
        Ok(path)
    })
}

/// A buffer's `/rel` key in the folder, unless it is outside it or private.
fn key(buffer: &Entity<Buffer>, root: WorktreeId, cx: &App) -> Option<PathBuf> {
    let file = project::File::from_dyn(buffer.read(cx).file())?;
    (file.worktree_id(cx) == root && !file.is_private)
        .then(|| PathBuf::from(format!("/{}", file.path.as_unix_str())))
}

/// Answer a code-intelligence read (`definitions`, `references`, `symbols`)
/// from Zed's language servers, through its LspStore (ADR-0048).
pub(crate) fn lsp_read(
    host: &HostTools,
    project: &Entity<Project>,
    uri: &CedianUri,
    cx: &mut App,
) -> Task<Result<HostUriRead, String>> {
    let Some(root) = project.read(cx).visible_worktrees(cx).next() else {
        return Task::ready(Err("no folder is open".to_string()));
    };
    let root_id = root.read(cx).id();
    let root_path = root.read(cx).abs_path();
    let (file, position) = match uri.kind {
        UriKind::Symbols => match uri.path.strip_prefix("file/") {
            Some(file) => (file, None),
            None => {
                let symbols = project.update(cx, |p, cx| p.symbols(&uri.path, cx));
                return cx.background_spawn(async move {
                    let symbols = symbols.await.map_err(|e| e.to_string())?;
                    Ok(symbols
                        .iter()
                        .map(|s| workspace_symbol(s, root_id))
                        .collect::<Vec<_>>()
                        .join("\n")
                        .into())
                });
            }
        },
        _ => match parse_position(&uri.path) {
            Some((file, position)) => (file, Some(position)),
            None => {
                return Task::ready(Err(format!(
                    "{} wants <path>:<line>:<column>, got {:?}",
                    uri_kind_name(&uri.kind),
                    uri.path
                )));
            }
        },
    };
    let path = match host.resolve(Path::new(file)) {
        Ok(key) => root_path.join(key.strip_prefix("/").unwrap_or(&key)),
        Err(e) => return Task::ready(Err(e)),
    };
    let kind = uri.kind.clone();
    let project = project.clone();
    cx.spawn(async move |cx| {
        let buffer = project
            .update(cx, |p, cx| p.open_local_buffer(&path, cx))
            .await
            .map_err(|e| e.to_string())?;
        // A file nobody has open has no language server until registered.
        let _lsp = project.update(cx, |p, cx| {
            p.register_buffer_with_language_servers(&buffer, cx)
        });
        let lines = match (kind, position) {
            (UriKind::Definitions, Some(at)) => {
                let links = project
                    .update(cx, |p, cx| p.definitions(&buffer, at, cx))
                    .await
                    .map_err(|e| e.to_string())?
                    .unwrap_or_default();
                cx.update(|cx| {
                    links
                        .iter()
                        .map(|link| location(&link.target, root_id, cx))
                        .collect::<Vec<_>>()
                })
            }
            (UriKind::References, Some(at)) => {
                let locations = project
                    .update(cx, |p, cx| p.references(&buffer, at, cx))
                    .await
                    .map_err(|e| e.to_string())?
                    .unwrap_or_default();
                cx.update(|cx| {
                    locations
                        .iter()
                        .map(|l| location(l, root_id, cx))
                        .collect::<Vec<_>>()
                })
            }
            _ => {
                let symbols = project
                    .update(cx, |p, cx| p.document_symbols(&buffer, cx))
                    .await
                    .map_err(|e| e.to_string())?;
                let mut lines = Vec::new();
                document_symbols(&symbols, &mut lines);
                lines
            }
        };
        Ok(lines.join("\n").into())
    })
}

/// `name (Kind) path:line:column`, the path as a `/rel` key when in the
/// folder.
fn workspace_symbol(symbol: &project::Symbol, root: WorktreeId) -> String {
    use project::lsp_store::SymbolLocation;
    let path = match &symbol.path {
        SymbolLocation::InProject(p) if p.worktree_id == root => {
            format!("/{}", p.path.as_unix_str())
        }
        SymbolLocation::InProject(p) => p.path.as_unix_str().to_string(),
        SymbolLocation::OutsideProject { abs_path, .. } => abs_path.display().to_string(),
    };
    let start = symbol.range.start.0;
    format!(
        "{} ({:?}) {path}:{}:{}",
        symbol.name, symbol.kind, start.row, start.column
    )
}

fn uri_kind_name(kind: &UriKind) -> &'static str {
    match kind {
        UriKind::Definitions => "cedian://definitions",
        UriKind::References => "cedian://references",
        _ => "cedian://symbols",
    }
}

/// `<path>:<line>:<column>`, both 0-based.
fn parse_position(path: &str) -> Option<(&str, PointUtf16)> {
    let mut parts = path.rsplitn(3, ':');
    let column = parts.next()?.parse().ok()?;
    let line = parts.next()?.parse().ok()?;
    Some((parts.next()?, PointUtf16::new(line, column)))
}

/// `path:line:column` of a location's start, the path as a `/rel` key when
/// in the folder.
fn location(location: &Location, root: WorktreeId, cx: &App) -> String {
    let buffer = location.buffer.read(cx);
    let start = location.range.start.to_point(&buffer.snapshot());
    let path = key(&location.buffer, root, cx)
        .map(|k| k.display().to_string())
        .or_else(|| buffer.file().map(|f| f.full_path(cx).display().to_string()))
        .unwrap_or_default();
    format!("{path}:{}:{}", start.row, start.column)
}

/// `name (Kind) line:column`, children indented under their parent.
fn document_symbols(symbols: &[project::DocumentSymbol], lines: &mut Vec<String>) {
    fn walk(symbols: &[project::DocumentSymbol], depth: usize, lines: &mut Vec<String>) {
        for s in symbols {
            let start = s.range.start.0;
            lines.push(format!(
                "{}{} ({:?}) {}:{}",
                "  ".repeat(depth),
                s.name,
                s.kind,
                start.row,
                start.column
            ));
            walk(&s.children, depth + 1, lines);
        }
    }
    walk(symbols, 0, lines);
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
    let key = |buffer: &Entity<Buffer>| key(buffer, root_id, cx);
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use language::{FakeLspAdapter, rust_lang};
    use settings::SettingsStore;

    const MAIN: &str = "fn helper() {}\nfn main() { helper(); }\n";

    fn at(line: u32, column: u32) -> lsp::Location {
        lsp::Location::new(
            lsp::Uri::from_file_path("/ws/src/main.rs").unwrap(),
            lsp::Range::new(
                lsp::Position::new(line, column),
                lsp::Position::new(line, column + 6),
            ),
        )
    }

    fn init(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    /// A real folder: `src/main.rs`, a private `.env`, and `link.txt`, a
    /// symlink to a file outside the folder.
    async fn real_folder(name: &str, cx: &mut TestAppContext) -> (PathBuf, Entity<Project>) {
        cx.executor().allow_parking();
        init(cx);
        let dir = std::env::temp_dir().join(format!("cedian-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ws/src")).unwrap();
        std::fs::create_dir_all(dir.join("outside")).unwrap();
        std::fs::write(dir.join("ws/src/main.rs"), MAIN).unwrap();
        std::fs::write(dir.join("ws/.env"), "SECRET=1\n").unwrap();
        std::fs::write(dir.join("outside/secret.txt"), "secret\n").unwrap();
        std::os::unix::fs::symlink(dir.join("outside/secret.txt"), dir.join("ws/link.txt"))
            .unwrap();
        let dir = dir.canonicalize().unwrap();
        let project = Project::test(
            fs::RealFs::new(None, cx.executor()),
            [dir.join("ws").as_path()],
            cx,
        )
        .await;
        let scan = project.read_with(cx, |p, cx| {
            let tree = p.visible_worktrees(cx).next().unwrap();
            tree.read(cx).as_local().unwrap().scan_complete()
        });
        scan.await;
        (dir, project)
    }

    /// `cedian://buffer` is answered from Zed only: never a private file,
    /// never a file a symlink in the folder points to outside it.
    #[gpui::test]
    async fn buffer_reads_refuse_private_files_and_links_outside(cx: &mut TestAppContext) {
        let (dir, project) = real_folder("u6-buffer-guard", cx).await;
        let read = |url: &'static str, cx: &mut TestAppContext| {
            let project = project.clone();
            cx.update(move |cx| answer(url, None, &project, cx))
        };
        let main = read("cedian://buffer/src/main.rs", cx).await.unwrap();
        assert_eq!(main.content, MAIN);
        let private = read("cedian://buffer/.env", cx).await.unwrap_err();
        assert!(private.contains("private"), "{private}");
        let link = read("cedian://buffer/link.txt", cx).await.unwrap_err();
        assert!(link.contains("outside"), "{link}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Definitions, references and symbols are Zed's language server's
    /// answers, asked through the LspStore for the position the URI names.
    #[gpui::test]
    async fn code_intelligence_reads_come_from_zeds_language_server(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/ws", serde_json::json!({"src": {"main.rs": MAIN}}))
            .await;
        let project = Project::test(fs, [Path::new("/ws")], cx).await;
        let languages = project.read_with(cx, |p, _| p.languages().clone());
        languages.add(rust_lang());
        let mut servers = languages.register_fake_lsp(
            "Rust",
            FakeLspAdapter {
                capabilities: lsp::ServerCapabilities {
                    definition_provider: Some(lsp::OneOf::Left(true)),
                    references_provider: Some(lsp::OneOf::Left(true)),
                    workspace_symbol_provider: Some(lsp::OneOf::Left(true)),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let read = |url: &'static str, cx: &mut TestAppContext| {
            let project = project.clone();
            cx.update(move |cx| {
                let host = host(None, &project, cx).unwrap();
                let uri = cedian_workspace::parse_cedian_uri(url).unwrap();
                lsp_read(&host, &project, &uri, cx)
            })
        };
        let (_buffer, _handle) = project
            .update(cx, |p, cx| {
                p.open_local_buffer_with_lsp("/ws/src/main.rs", cx)
            })
            .await
            .unwrap();
        let server = servers.next().await.unwrap();
        server.set_request_handler::<lsp::request::GotoDefinition, _, _>(|params, _| async move {
            let at_position = params.text_document_position_params.position;
            assert_eq!(at_position, lsp::Position::new(1, 12));
            Ok(Some(lsp::GotoDefinitionResponse::Scalar(at(0, 3))))
        });
        server.set_request_handler::<lsp::request::References, _, _>(|params, _| async move {
            assert_eq!(
                params.text_document_position.position,
                lsp::Position::new(1, 12)
            );
            Ok(Some(vec![at(0, 3), at(1, 12)]))
        });
        #[allow(deprecated)]
        server.set_request_handler::<lsp::request::WorkspaceSymbolRequest, _, _>(
            |params, _| async move {
                assert_eq!(params.query, "help");
                Ok(Some(lsp::WorkspaceSymbolResponse::Flat(vec![
                    lsp::SymbolInformation {
                        name: "helper".to_string(),
                        kind: lsp::SymbolKind::FUNCTION,
                        tags: None,
                        deprecated: None,
                        location: at(0, 3),
                        container_name: None,
                    },
                ])))
            },
        );
        cx.run_until_parked();
        let definitions = read("cedian://definitions/src/main.rs:1:12", cx);
        assert_eq!(definitions.await.unwrap().content, "/src/main.rs:0:3");
        let references = read("cedian://references/src/main.rs:1:12", cx);
        assert_eq!(
            references.await.unwrap().content,
            "/src/main.rs:0:3\n/src/main.rs:1:12"
        );
        let symbols = read("cedian://symbols/help", cx);
        assert_eq!(
            symbols.await.unwrap().content,
            "helper (Function) /src/main.rs:0:3"
        );
        let outside = read("cedian://definitions/../etc/passwd:0:0", cx);
        assert!(outside.await.unwrap_err().contains("escapes"));
        let shapeless = read("cedian://references/src/main.rs", cx);
        assert!(
            shapeless
                .await
                .unwrap_err()
                .contains("<path>:<line>:<column>")
        );
    }
}

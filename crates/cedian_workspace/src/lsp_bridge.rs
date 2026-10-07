//! LSP bridge: `LspClient` → `HostTools` (S1).
//!
//! Owns one shared `LspClient` per workspace root, syncs open buffers into it
//! (`didOpen`), and pumps `publishDiagnostics` into
//! `HostTools.publish_diagnostics` on a background thread. Serves
//! `cedian://symbols/<query>` (workspace) + `cedian://symbols/file/<path>`
//! (document) from live server data.
//!
//! Blocking calls run on the caller's thread (dedicated threads, never UI).
//! DAP side (breakpoints/stack/vars) lives in `cedian_dap`; its bridge lands
//! once Developer mode is enabled on the machine (S1 follow-up).

use cedian_lsp::{LspClient, LspDiagnostic, LspError, LspNotification, LspSymbol};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::{Diagnostic, DiagnosticSeverity, HostTools};

/// Bridge failures (caller-visible).
#[derive(Debug)]
pub enum LspBridgeError {
    Lsp(LspError),
    NoBridge { workdir: PathBuf },
}

impl std::fmt::Display for LspBridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lsp(e) => write!(f, "lsp bridge: {e}"),
            Self::NoBridge { workdir } => write!(f, "no LSP bridge for {}", workdir.display()),
        }
    }
}

impl std::error::Error for LspBridgeError {}

impl From<LspError> for LspBridgeError {
    fn from(e: LspError) -> Self {
        Self::Lsp(e)
    }
}

/// Convert an LSP file URI to a local path (`file://` only).
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    uri.strip_prefix("file://").map(PathBuf::from)
}

/// Convert a local path to an LSP file URI.
pub fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// Map LSP severity (1=error..4=hint) to cedian severity.
fn map_severity(sev: Option<u32>) -> DiagnosticSeverity {
    match sev {
        Some(1) => DiagnosticSeverity::Error,
        Some(2) => DiagnosticSeverity::Warning,
        _ => DiagnosticSeverity::Info,
    }
}

fn map_diagnostic(path: PathBuf, d: &LspDiagnostic) -> Diagnostic {
    Diagnostic {
        path,
        line: d.range.start.line as usize,
        severity: map_severity(d.severity),
        message: d.message.lines().next().unwrap_or("").to_string(),
    }
}

/// One workspace's LSP session: shared client + diagnostics pump thread.
pub struct LspBridge {
    client: Arc<LspClient>,
    workdir: PathBuf,
}

impl LspBridge {
    /// Spawn the server for `workdir` and start the diagnostics pump into
    /// `host`. `server_cmd` is the LSP binary (default `rust-analyzer`).
    /// The pump thread lives until the bridge drops (client EOF ends it).
    pub fn spawn(
        server_cmd: &str,
        workdir: &Path,
        host: &Arc<HostTools>,
    ) -> Result<Self, LspBridgeError> {
        Self::spawn_inner(None, server_cmd, workdir, host)
    }

    /// Spawn with an explicit argv (fakes, non-rust-analyzer servers — no
    /// `--log-file` flag injection).
    pub fn spawn_argv(
        argv: &[&str],
        workdir: &Path,
        host: &Arc<HostTools>,
    ) -> Result<Self, LspBridgeError> {
        let (prog, rest) = argv.split_first().ok_or_else(|| {
            LspBridgeError::Lsp(cedian_lsp::LspError::Spawn("empty argv".to_string()))
        })?;
        Self::spawn_inner(Some(rest), prog, workdir, host)
    }

    fn spawn_inner(
        extra_args: Option<&[&str]>,
        server_cmd: &str,
        workdir: &Path,
        host: &Arc<HostTools>,
    ) -> Result<Self, LspBridgeError> {
        let root_uri = path_to_uri(workdir);
        let client = Arc::new(match extra_args {
            Some(args) => {
                let mut argv = vec![server_cmd];
                argv.extend_from_slice(args);
                LspClient::spawn_argv(&argv, &workdir.to_string_lossy(), &root_uri)?
            }
            None => LspClient::spawn(server_cmd, &workdir.to_string_lossy(), &root_uri)?,
        });
        let pump_client = Arc::clone(&client);
        let pump_host = Arc::clone(host);
        std::thread::spawn(move || {
            // ONE subscription for the pump's lifetime. Loop FOREVER (the
            // thread dies with the process): a recv timeout only means an
            // idle second — NOT end of stream. (`while let Ok` would exit on
            // the first quiet second and drop all later waves.)
            let rx = pump_client.subscribe();
            loop {
                match rx.recv_timeout(Duration::from_secs(1)) {
                    Ok(notif) => {
                        if std::env::var("CEDIAN_LSP_PUMP_LOG").is_ok() {
                            eprintln!("[pump] notif: {}", notif_summary(&notif));
                        }
                        if let LspNotification::Diagnostics { uri, diagnostics } = notif {
                            let Some(local) = uri_to_path(&uri) else {
                                eprintln!("[pump] SKIP uri not file:// : {uri}");
                                continue;
                            };
                            // Normalize to the workspace KEY (not the absolute
                            // path): diagnostics and buffers share one namespace.
                            let key = match pump_host.resolve(&local) {
                                Ok(key) => key,
                                Err(e) => {
                                    eprintln!("[pump] SKIP resolve failed: {e}");
                                    continue;
                                }
                            };
                            let mapped: Vec<Diagnostic> = diagnostics
                                .iter()
                                .map(|d| map_diagnostic(key.clone(), d))
                                .collect();
                            pump_host.publish_diagnostics(&key, mapped);
                        }
                    }
                    Err(_) => continue,
                }
            }
        });
        Ok(Self {
            client,
            workdir: workdir.to_path_buf(),
        })
    }

    /// Sync one open buffer's text into the server (didOpen + didSave to
    /// trigger check-on-save diagnostics, then the pump collects them).
    pub fn sync_buffer(&self, local_path: &Path, text: &str) {
        let uri = path_to_uri(local_path);
        self.client
            .did_open(&uri, language_id_for(local_path), 1, text);
        self.client.did_save(&uri, None);
    }

    /// Workspace symbol search (`cedian://symbols/<query>`).
    pub fn workspace_symbols(&self, query: &str) -> Result<Vec<LspSymbol>, LspBridgeError> {
        Ok(self.client.workspace_symbols(query)?)
    }

    /// Document symbols (`cedian://symbols/file/<path>`).
    pub fn document_symbols(&self, local_path: &Path) -> Result<Vec<LspSymbol>, LspBridgeError> {
        Ok(self.client.document_symbols(&path_to_uri(local_path))?)
    }

    /// Workspace root.
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }
}

/// One-line pump log summary (gated by `CEDIAN_LSP_PUMP_LOG`).
fn notif_summary(notif: &cedian_lsp::LspNotification) -> String {
    match notif {
        cedian_lsp::LspNotification::Diagnostics { uri, diagnostics } => {
            format!("diagnostics {uri} ({} items)", diagnostics.len())
        }
        cedian_lsp::LspNotification::Other { method } => format!("other {method}"),
    }
}

fn language_id_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "rs" => "rust",
        "c" | "h" => "c",
        "py" => "python",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" => "javascript",
        _ => "plaintext",
    }
}

/// Render workspace symbols as `name (kind) @ container` lines.
pub fn render_workspace_symbols(symbols: &[LspSymbol]) -> String {
    symbols
        .iter()
        .map(|s| {
            let container = s.container.as_deref().unwrap_or("");
            if container.is_empty() {
                format!("{} ({})", s.name, s.kind)
            } else {
                format!("{} ({}) @ {container}", s.name, s.kind)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_roundtrip() {
        let p = Path::new("/tmp/x/src/main.rs");
        assert_eq!(uri_to_path(&path_to_uri(p)), Some(p.to_path_buf()));
        assert!(uri_to_path("cedian://buffer/x").is_none());
    }

    #[test]
    fn severity_mapping() {
        assert_eq!(map_severity(Some(1)), DiagnosticSeverity::Error);
        assert_eq!(map_severity(Some(2)), DiagnosticSeverity::Warning);
        assert_eq!(map_severity(None), DiagnosticSeverity::Info);
    }
}

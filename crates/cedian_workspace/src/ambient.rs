//! Ambient context snapshot (plan §39): small pre-prompt context packet.
//!
//! Before the prompt, cedian sends active file + selection + diagnostics
//! summary — small, bounded, never the full transcript. Deep context stays
//! on-demand via `cedian://` URIs (§40). Snapshot is plain data: the owner
//! decides how to inject it (system prompt prefix, hidden user companion, …).

use crate::{Diagnostic, WorkspaceHost};
use serde::{Deserialize, Serialize};

/// Bounded ambient snapshot for one prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AmbientSnapshot {
    pub active_file: Option<String>,
    /// `path:start-end` + selected text (truncated).
    pub selection: Option<String>,
    /// `severity path:line message` lines (truncated count).
    pub diagnostics: Vec<String>,
}

/// Max selected-text chars in the snapshot (deep text via `cedian://selection`).
pub const MAX_SELECTION_CHARS: usize = 500;
/// Max diagnostics lines in the snapshot (full list via `cedian://diagnostics`).
pub const MAX_DIAGNOSTICS_LINES: usize = 20;
/// Max chars of one diagnostic line in the snapshot.
pub const MAX_DIAGNOSTIC_CHARS: usize = 200;

/// Buffer key (`/rel`) as the agent should name it: workspace-relative.
/// OMP runs with the workspace as cwd and reads `/rel` as an absolute path.
fn rel(path: &std::path::Path) -> String {
    path.to_string_lossy().trim_start_matches('/').to_string()
}

/// Capture the ambient snapshot from the host. Cheap, bounded, infallible
/// (empty when no editor state — never an error).
pub fn capture_ambient(host: &dyn WorkspaceHost) -> AmbientSnapshot {
    let active_file = host.active_file().map(|p| rel(&p));
    let selection = host.selection().and_then(|(path, start, end)| {
        let text = host.read_buffer(&path)?;
        let end = end.min(text.len());
        let start = start.min(end);
        let mut snippet: String = text[start..end].chars().take(MAX_SELECTION_CHARS).collect();
        if text[start..end].chars().count() > MAX_SELECTION_CHARS {
            snippet.push('…');
        }
        Some(format!("{} bytes {start}-{end}\n{snippet}", rel(&path)))
    });
    // The active file's, most severe first, then earliest.
    let mut found = host
        .active_file()
        .map(|active| host.diagnostics(&active))
        .unwrap_or_default();
    found.sort_by_key(|d| (d.severity, d.line));
    AmbientSnapshot {
        active_file,
        selection,
        diagnostics: found
            .iter()
            .take(MAX_DIAGNOSTICS_LINES)
            .map(render_diagnostic)
            .collect(),
    }
}

fn render_diagnostic(d: &Diagnostic) -> String {
    let sev = match d.severity {
        crate::DiagnosticSeverity::Error => "error",
        crate::DiagnosticSeverity::Warning => "warning",
        crate::DiagnosticSeverity::Info => "info",
    };
    let message = d.message.split_whitespace().collect::<Vec<_>>().join(" ");
    let line = format!("{sev} {}:{} {message}", rel(&d.path), d.line);
    if line.chars().count() <= MAX_DIAGNOSTIC_CHARS {
        return line;
    }
    let mut short: String = line.chars().take(MAX_DIAGNOSTIC_CHARS - 1).collect();
    short.push('…');
    short
}

/// Render the snapshot as a compact pre-prompt block. Empty sections omitted.
pub fn render_snapshot(snapshot: &AmbientSnapshot) -> String {
    let mut out = String::new();
    if let Some(file) = &snapshot.active_file {
        out.push_str(&format!("active-file: {file}\n"));
    }
    if let Some(sel) = &snapshot.selection {
        out.push_str(&format!("selection:\n{sel}\n"));
    }
    if !snapshot.diagnostics.is_empty() {
        out.push_str("diagnostics:\n");
        for line in &snapshot.diagnostics {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HostTools;
    use std::path::Path;
    use std::path::PathBuf;

    #[test]
    fn empty_host_empty_snapshot() {
        let host = HostTools::new(Path::new("/"));
        let snap = capture_ambient(&host);
        assert!(snap.active_file.is_none());
        assert!(snap.selection.is_none());
        assert!(render_snapshot(&snap).is_empty());
    }

    #[test]
    fn selection_and_diagnostics_captured() {
        let host = HostTools::new(Path::new("/"));
        host.open(Path::new("/a.rs"), "fn main() {}\n");
        host.set_active_file(Some(PathBuf::from("/a.rs")));
        host.set_selection(Some((PathBuf::from("/a.rs"), 0, 2)));
        host.publish_diagnostics(
            Path::new("/a.rs"),
            vec![Diagnostic {
                path: PathBuf::from("/a.rs"),
                line: 0,
                severity: crate::DiagnosticSeverity::Error,
                message: "boom".to_string(),
            }],
        );
        let snap = capture_ambient(&host);
        assert_eq!(snap.active_file.as_deref(), Some("a.rs"));
        assert!(snap.selection.as_deref().unwrap_or("").contains("fn"));
        assert_eq!(snap.diagnostics.len(), 1);
        let rendered = render_snapshot(&snap);
        assert!(rendered.contains("active-file: a.rs"));
        assert!(rendered.contains("selection:\na.rs bytes 0-2\nfn"));
        assert!(rendered.contains("error a.rs:0 boom"));
    }

    /// The 20 kept are the most severe, then the earliest, each one line of
    /// at most 200 chars.
    #[test]
    fn snapshot_keeps_the_worst_diagnostics_on_one_short_line_each() {
        use crate::DiagnosticSeverity::{Error, Info, Warning};
        let host = HostTools::new(Path::new("/"));
        host.open(Path::new("/a.rs"), "");
        host.set_active_file(Some(PathBuf::from("/a.rs")));
        let at = |line, severity, message: &str| Diagnostic {
            path: PathBuf::from("/a.rs"),
            line,
            severity,
            message: message.to_string(),
        };
        let mut diagnostics: Vec<_> = (0..25).map(|line| at(line, Info, "hint")).collect();
        diagnostics.push(at(40, Error, "late"));
        diagnostics.push(at(30, Error, "early"));
        diagnostics.push(at(35, Warning, &format!("first\n{}", "x".repeat(300))));
        host.publish_diagnostics(Path::new("/a.rs"), diagnostics);
        let snap = capture_ambient(&host);
        assert_eq!(snap.diagnostics.len(), MAX_DIAGNOSTICS_LINES);
        assert_eq!(snap.diagnostics[0], "error a.rs:30 early");
        assert_eq!(snap.diagnostics[1], "error a.rs:40 late");
        assert!(snap.diagnostics[2].starts_with("warning a.rs:35 first x"));
        assert_eq!(snap.diagnostics[3], "info a.rs:0 hint");
        for line in &snap.diagnostics {
            assert!(!line.contains('\n') && line.chars().count() <= MAX_DIAGNOSTIC_CHARS);
        }
    }
}

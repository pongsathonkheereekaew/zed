//! Workspace file effects of OMP's own tools, for record/replay (row G).
//!
//! Record: snapshot the workspace at `tool_execution_start`, diff at
//! `tool_execution_end`, emit one `fs` record per file the tool changed —
//! so edits the host made between tool calls (write-back, a test's "user
//! edit") are never mistaken for OMP's. Replay: apply each `fs` record to
//! disk where it was recorded, just before the `tool_execution_end` frame.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

/// Files bigger than this are not tracked (fixtures stay small).
const MAX_BYTES: u64 = 256 * 1024;

/// Relative path → text, for every small UTF-8 file under `root`, skipping
/// dot-directories (`.git`, `.worktrees`, any `.name`).
pub fn snapshot(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if !name.to_string_lossy().starts_with('.') {
                walk(root, &path, out);
            }
            continue;
        }
        if !kind.is_file() || entry.metadata().map_or(true, |m| m.len() > MAX_BYTES) {
            continue;
        }
        let (Ok(rel), Ok(text)) = (path.strip_prefix(root), std::fs::read_to_string(&path)) else {
            continue;
        };
        out.insert(rel.to_string_lossy().into_owned(), text);
    }
}

/// `fs` frames turning `before` into `after`, in path order.
pub fn diff(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) -> Vec<Value> {
    let mut frames = Vec::new();
    for (path, text) in after {
        if before.get(path) != Some(text) {
            frames.push(json!({"type": "fs_write", "path": path, "content": text}));
        }
    }
    for path in before.keys().filter(|p| !after.contains_key(*p)) {
        frames.push(json!({"type": "fs_delete", "path": path}));
    }
    frames
}

/// Apply one `fs` frame under `root`. Paths escaping `root` are refused.
pub fn apply(root: &Path, frame: &Value) -> Result<(), String> {
    let rel = frame
        .get("path")
        .and_then(Value::as_str)
        .ok_or("fs frame without path")?;
    if Path::new(rel)
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(format!("fs frame path escapes the workspace: {rel}"));
    }
    let path = root.join(rel);
    match frame.get("type").and_then(Value::as_str) {
        Some("fs_write") => {
            let text = frame.get("content").and_then(Value::as_str).unwrap_or("");
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            std::fs::write(&path, text).map_err(|e| e.to_string())
        }
        Some("fs_delete") => match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        },
        other => Err(format!("unknown fs frame {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_diff_apply_roundtrip() {
        let root = std::env::temp_dir().join(format!("cedian-fsfx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        std::fs::write(root.join("gone.txt"), "x\n").unwrap();
        std::fs::write(root.join(".hidden/state.json"), "{}").unwrap();
        let before = snapshot(&root);
        assert!(
            !before.contains_key(".hidden/state.json"),
            "dot dirs skipped"
        );
        std::fs::write(root.join("a.txt"), "ONE\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/new.rs"), "fn f() {}\n").unwrap();
        std::fs::remove_file(root.join("gone.txt")).unwrap();
        let frames = diff(&before, &snapshot(&root));
        assert_eq!(frames.len(), 3, "{frames:?}");

        // Replay onto the original state.
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        std::fs::remove_file(root.join("src/new.rs")).unwrap();
        std::fs::write(root.join("gone.txt"), "x\n").unwrap();
        for f in &frames {
            apply(&root, f).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "ONE\n"
        );
        assert!(root.join("src/new.rs").exists());
        assert!(!root.join("gone.txt").exists());
        assert!(
            apply(
                &root,
                &json!({"type": "fs_write", "path": "../x", "content": ""})
            )
            .is_err()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}

//! Workspace file scan for the CLI harness: text files ↔ buffer keys.
//!
//! Buffer keys are absolute `/{rel}` paths (e.g. `/src/main.rs`); local paths
//! are `workdir + rel`. Binary/ignored files are skipped by extension + size.
//! The app shell will replace this with a real project scan (gitignore-aware).

use std::path::{Path, PathBuf};

/// Max file size to load (1 MiB — larger files are agent-opaque for now).
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Skip these dir names everywhere.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".cedian",
    "target",
    "node_modules",
    ".worktrees",
    "DerivedData",
];
/// Skip these extensions (binary/lock).
const SKIP_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "ico", "lock", "bin", "o", "a", "dylib", "so",
];

/// Recursively collect text files under workdir (sorted, bounded).
pub fn scan_text_files(workdir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    scan_dir(workdir, true, &mut out);
    out.sort();
    out
}

/// Every file evidence can depend on: lockfiles, binaries and large files
/// too, so a change to any of them makes tree-bound evidence stale.
pub fn scan_code_state_files(workdir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    scan_dir(workdir, false, &mut out);
    out.sort();
    out
}

fn scan_dir(dir: &Path, text_only: bool, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir)
        .map(|r| r.collect::<Vec<_>>())
        .unwrap_or_default();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                scan_dir(&path, text_only, out);
            }
            continue;
        }
        if !text_only {
            out.push(path);
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        if SKIP_EXTS.contains(&ext.as_str()) {
            continue;
        }
        if entry.metadata().map(|m| m.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
            continue;
        }
        out.push(path);
    }
}

/// Buffer key for a local path: `/` + workdir-relative path. `None` when the
/// path escapes workdir.
pub fn buffer_key(workdir: &Path, local: &Path) -> Option<PathBuf> {
    let rel = local.strip_prefix(workdir).ok()?;
    Some(PathBuf::from(format!("/{}", rel.display())))
}

/// Local path for a buffer key under workdir. `None` on escape.
pub fn local_path(workdir: &Path, key: &Path) -> Option<PathBuf> {
    let rel = key.strip_prefix("/").ok()?;
    if rel
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    Some(workdir.join(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_roundtrip() {
        let work = Path::new("/tmp/w");
        let local = Path::new("/tmp/w/src/a.rs");
        let key = buffer_key(work, local).unwrap();
        assert_eq!(key, PathBuf::from("/src/a.rs"));
        assert_eq!(local_path(work, &key), Some(local.to_path_buf()));
        assert!(local_path(work, Path::new("/../evil")).is_none());
        assert!(buffer_key(work, Path::new("/elsewhere/x")).is_none());
    }
}

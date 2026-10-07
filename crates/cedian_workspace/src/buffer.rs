//! In-memory buffer store: versioned text + transactions + undo.
//!
//! Headless stand-in for Zed `text::Buffer` + `History`. Versions are a
//! monotonic `u64` per buffer here; the Zed binding replaces [`Version`] with
//! `clock::Global` without changing call shapes (plan §12: `buffer_version` +
//! `apply_edit(path, expected, edit)`).
//!
//! Transaction discipline mirrors `History::{start,end,push}`: `apply_edit`
//! checks `expected_version` (optimistic concurrency — concurrent edits fail
//! closed, never silently merge), pushes the inverse for undo, bumps the
//! version exactly once per applied edit.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Buffer version. `u64` headless; `clock::Global` under Zed (same order +
/// equality semantics the trait needs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Version(pub u64);

/// One text replacement: byte range → new text. Ranges are validated against
/// the current buffer; out-of-bounds or misordered ranges fail closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    /// Byte offset of the range start (inclusive).
    pub start: usize,
    /// Byte offset of the range end (exclusive, `>= start`).
    pub end: usize,
    /// Replacement text.
    pub replacement: String,
}

/// Outcome of one applied edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyEditResult {
    /// Version after the edit.
    pub new_version: Version,
    /// Whether the buffer differs from the last save.
    pub dirty: bool,
}

/// Buffer failures (caller-visible; undo of a clean stack is a no-op, not an error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BufferError {
    /// Expected version ≠ current (concurrent edit — re-read and retry).
    VersionMismatch { expected: Version, current: Version },
    /// Range outside the buffer or `end < start`.
    BadRange {
        start: usize,
        end: usize,
        len: usize,
    },
    /// Offsets split a UTF-8 code point.
    NotCharBoundary { offset: usize },
}

impl std::fmt::Display for BufferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VersionMismatch { expected, current } => {
                write!(
                    f,
                    "buffer changed under edit (expected v{}, now v{})",
                    expected.0, current.0
                )
            }
            Self::BadRange { start, end, len } => {
                write!(f, "bad edit range {start}..{end} for buffer len {len}")
            }
            Self::NotCharBoundary { offset } => write!(f, "offset {offset} splits a code point"),
        }
    }
}

impl std::error::Error for BufferError {}

#[derive(Debug, Clone)]
struct Buffer {
    text: String,
    version: Version,
    saved_version: Version,
    /// Inverse edits for undo, newest last. One entry per applied edit.
    undo: Vec<InverseEdit>,
}

#[derive(Debug, Clone)]
struct InverseEdit {
    start: usize,
    end: usize,
    original: String,
}

/// Versioned text buffers with transactions + undo. Single-owner (`&mut`);
/// sharing is the host's job (parking_lot wrapper there, not here).
#[derive(Debug, Default)]
pub struct BufferStore {
    buffers: HashMap<PathBuf, Buffer>,
}

impl BufferStore {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Open (or create) a buffer with initial text at v0, clean.
    pub fn open(&mut self, path: &Path, text: &str) {
        self.buffers.entry(path.to_path_buf()).or_insert(Buffer {
            text: text.to_string(),
            version: Version(0),
            saved_version: Version(0),
            undo: Vec::new(),
        });
    }

    /// Open, or take the disk text when it differs from a clean buffer
    /// (headless form of Zed's `Buffer::reload`, row G: disk is authoritative
    /// for OMP's own writes). A changed text is a new version, saved, with
    /// the undo stack cleared (its offsets point into the old text). A dirty
    /// buffer is never overwritten; returns false then.
    pub fn reload(&mut self, path: &Path, text: &str) -> bool {
        let Some(buf) = self.buffers.get_mut(path) else {
            self.open(path, text);
            return true;
        };
        if buf.text == text {
            return true;
        }
        if buf.version != buf.saved_version {
            return false;
        }
        buf.text = text.to_string();
        buf.version = Version(buf.version.0 + 1);
        buf.saved_version = buf.version;
        buf.undo.clear();
        true
    }

    /// Current text + version. Fails when the buffer was never opened.
    pub fn read(&self, path: &Path) -> Option<(String, Version)> {
        self.buffers.get(path).map(|b| (b.text.clone(), b.version))
    }

    /// Current version (for `expected_version` reads).
    pub fn version(&self, path: &Path) -> Option<Version> {
        self.buffers.get(path).map(|b| b.version)
    }

    /// Open buffer paths (for `cedian://open-editors`).
    pub fn open_paths(&self) -> Vec<PathBuf> {
        self.buffers.keys().cloned().collect()
    }

    /// Whether the buffer differs from the last save.
    pub fn is_dirty(&self, path: &Path) -> bool {
        self.buffers
            .get(path)
            .map(|b| b.version != b.saved_version)
            .unwrap_or(false)
    }

    /// Apply one edit transactionally: version check → range check → apply →
    /// push inverse → bump version. Dirty flag derives from versions.
    pub fn apply_edit(
        &mut self,
        path: &Path,
        expected: Version,
        edit: &TextEdit,
    ) -> Result<ApplyEditResult, BufferError> {
        let buf = self.buffers.get_mut(path).ok_or(BufferError::BadRange {
            start: edit.start,
            end: edit.end,
            len: 0,
        })?;
        if buf.version != expected {
            return Err(BufferError::VersionMismatch {
                expected,
                current: buf.version,
            });
        }
        check_range(&buf.text, edit)?;
        let original = buf.text[edit.start..edit.end].to_string();
        buf.text
            .replace_range(edit.start..edit.end, &edit.replacement);
        buf.undo.push(InverseEdit {
            start: edit.start,
            end: edit.start + edit.replacement.len(),
            original,
        });
        buf.version = Version(buf.version.0 + 1);
        Ok(ApplyEditResult {
            new_version: buf.version,
            dirty: buf.version != buf.saved_version,
        })
    }

    /// Undo the newest edit on one buffer. No-op on a clean stack.
    /// Returns the restored version, or `None` when nothing was undone.
    pub fn undo(&mut self, path: &Path) -> Option<Version> {
        let buf = self.buffers.get_mut(path)?;
        let inv = buf.undo.pop()?;
        buf.text.replace_range(inv.start..inv.end, &inv.original);
        buf.version = Version(buf.version.0 + 1);
        Some(buf.version)
    }

    /// Mark saved at the current version (Agent Sync: before turn start).
    pub fn mark_saved(&mut self, path: &Path) {
        if let Some(buf) = self.buffers.get_mut(path) {
            buf.saved_version = buf.version;
        }
    }

    /// Dirty buffers (Agent Sync scans this before the turn).
    pub fn dirty_buffers(&self) -> Vec<PathBuf> {
        self.buffers
            .iter()
            .filter(|(_, b)| b.version != b.saved_version)
            .map(|(p, _)| p.clone())
            .collect()
    }
}

fn check_range(text: &str, edit: &TextEdit) -> Result<(), BufferError> {
    if edit.end < edit.start || edit.end > text.len() {
        return Err(BufferError::BadRange {
            start: edit.start,
            end: edit.end,
            len: text.len(),
        });
    }
    if !text.is_char_boundary(edit.start) {
        return Err(BufferError::NotCharBoundary { offset: edit.start });
    }
    if !text.is_char_boundary(edit.end) {
        return Err(BufferError::NotCharBoundary { offset: edit.end });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_takes_disk_text_unless_dirty() {
        let mut store = BufferStore::new();
        let p = Path::new("/a");
        assert!(store.reload(p, "one\n"));
        assert!(store.reload(p, "two\n"));
        assert_eq!(store.read(p), Some(("two\n".to_string(), Version(1))));
        assert!(store.reload(p, "two\n"), "same text: no new version");
        assert_eq!(store.version(p), Some(Version(1)));
        store
            .apply_edit(
                p,
                Version(1),
                &TextEdit {
                    start: 0,
                    end: 3,
                    replacement: "TWO".into(),
                },
            )
            .unwrap();
        assert!(!store.reload(p, "three\n"), "dirty buffer kept");
        assert_eq!(store.read(p).unwrap().0, "TWO\n");
    }

    fn store() -> BufferStore {
        let mut s = BufferStore::new();
        s.open(Path::new("/a.rs"), "hello world");
        s
    }

    #[test]
    fn edit_bumps_version_and_dirties() {
        let mut s = store();
        let r = s
            .apply_edit(
                Path::new("/a.rs"),
                Version(0),
                &TextEdit {
                    start: 6,
                    end: 11,
                    replacement: "cedian".into(),
                },
            )
            .unwrap();
        assert_eq!(r.new_version, Version(1));
        assert!(r.dirty);
        assert_eq!(s.read(Path::new("/a.rs")).unwrap().0, "hello cedian");
    }

    #[test]
    fn version_mismatch_fails_closed() {
        let mut s = store();
        s.apply_edit(
            Path::new("/a.rs"),
            Version(0),
            &TextEdit {
                start: 0,
                end: 5,
                replacement: "bye".into(),
            },
        )
        .unwrap();
        let err = s
            .apply_edit(
                Path::new("/a.rs"),
                Version(0),
                &TextEdit {
                    start: 0,
                    end: 3,
                    replacement: "x".into(),
                },
            )
            .unwrap_err();
        assert_eq!(
            err,
            BufferError::VersionMismatch {
                expected: Version(0),
                current: Version(1)
            }
        );
        // Failed edit changed nothing.
        assert_eq!(s.read(Path::new("/a.rs")).unwrap().0, "bye world");
    }

    #[test]
    fn undo_restores_text() {
        let mut s = store();
        s.apply_edit(
            Path::new("/a.rs"),
            Version(0),
            &TextEdit {
                start: 0,
                end: 5,
                replacement: "bye".into(),
            },
        )
        .unwrap();
        s.undo(Path::new("/a.rs")).unwrap();
        assert_eq!(s.read(Path::new("/a.rs")).unwrap().0, "hello world");
        // Empty stack: no-op.
        s.undo(Path::new("/a.rs"));
        s.undo(Path::new("/a.rs"));
    }

    #[test]
    fn bad_range_and_boundary_fail() {
        let mut s = store();
        assert!(
            s.apply_edit(
                Path::new("/a.rs"),
                Version(0),
                &TextEdit {
                    start: 5,
                    end: 99,
                    replacement: String::new()
                }
            )
            .is_err()
        );
        let mut s2 = BufferStore::new();
        s2.open(Path::new("/u.rs"), "héllo");
        assert_eq!(
            s2.apply_edit(
                Path::new("/u.rs"),
                Version(0),
                &TextEdit {
                    start: 1,
                    end: 2,
                    replacement: String::new()
                }
            )
            .unwrap_err(),
            BufferError::NotCharBoundary { offset: 2 }
        );
    }

    #[test]
    fn save_clears_dirty() {
        let mut s = store();
        s.apply_edit(
            Path::new("/a.rs"),
            Version(0),
            &TextEdit {
                start: 0,
                end: 5,
                replacement: "bye".into(),
            },
        )
        .unwrap();
        assert!(s.is_dirty(Path::new("/a.rs")));
        s.mark_saved(Path::new("/a.rs"));
        assert!(!s.is_dirty(Path::new("/a.rs")));
        assert!(s.dirty_buffers().is_empty());
    }
}

//! Code state evidence is bound to (ADR-0024 decision 1).
//!
//! Headless form (ROADMAP stand-in row H, ADR-0036): a stable content hash
//! per file, or one fingerprint over the workspace tree for repo-wide checks
//! (a test suite run). In the app a file open in a Zed buffer with unsaved
//! edits is bound to that buffer's `clock::Global` instead
//! ([`CurrentState::overlay`], ADR-0057 decision 7).
//!
//! Staleness is pure: the caller hashes the workspace into a
//! [`CurrentState`] and passes it in; nothing here touches the disk.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What one evidence item verified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeState {
    /// Workspace-relative path → content hash of each file the call saw.
    Files(BTreeMap<String, u64>),
    /// Fingerprint of every workspace text file (repo-wide calls).
    Tree(u64),
}

/// The workspace as it is now: every text file's hash plus the tree
/// fingerprint over them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CurrentState {
    pub files: BTreeMap<String, u64>,
    pub tree: u64,
    /// The shared browser's current frame sequence, when it runs.
    pub frame_seq: Option<u64>,
}

/// FNV-1a 64: stable across processes and toolchains (unlike `DefaultHasher`).
pub fn content_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn tree_of(files: &BTreeMap<String, u64>) -> u64 {
    let mut flat = Vec::new();
    for (path, hash) in files {
        flat.extend_from_slice(path.as_bytes());
        flat.push(0);
        flat.extend_from_slice(&hash.to_le_bytes());
    }
    content_hash(&flat)
}

impl CurrentState {
    /// From `(relative path, contents)` pairs, in any order.
    pub fn from_files<'a>(files: impl IntoIterator<Item = (String, &'a [u8])>) -> Self {
        let files: BTreeMap<String, u64> = files
            .into_iter()
            .map(|(path, bytes)| (path, content_hash(bytes)))
            .collect();
        Self {
            tree: tree_of(&files),
            files,
            frame_seq: None,
        }
    }

    /// Unsaved buffers over the disk: each path's entry becomes its
    /// buffer's version token, so a keystroke changes it without a save.
    pub fn overlay<'a>(&mut self, unsaved: impl IntoIterator<Item = (&'a String, &'a String)>) {
        for (path, version) in unsaved {
            self.files
                .insert(path.clone(), content_hash(version.as_bytes()));
        }
        self.tree = tree_of(&self.files);
    }

    /// The state a call saw: the named files when it named any that exist,
    /// else the whole tree.
    pub fn bind(&self, paths: &[String]) -> CodeState {
        let named: BTreeMap<String, u64> = paths
            .iter()
            .filter_map(|p| self.files.get(p).map(|h| (p.clone(), *h)))
            .collect();
        if named.is_empty() {
            CodeState::Tree(self.tree)
        } else {
            CodeState::Files(named)
        }
    }

    /// `Some(reason)` when `state` no longer matches the workspace.
    pub fn stale_reason(&self, state: &CodeState) -> Option<String> {
        match state {
            CodeState::Tree(tree) => {
                (*tree != self.tree).then(|| "the workspace changed since".to_string())
            }
            CodeState::Files(files) => {
                let changed: Vec<&str> = files
                    .iter()
                    .filter(|(path, hash)| self.files.get(*path) != Some(*hash))
                    .map(|(path, _)| path.as_str())
                    .collect();
                (!changed.is_empty()).then(|| format!("{} changed since", changed.join(", ")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(files: &[(&str, &str)]) -> CurrentState {
        CurrentState::from_files(files.iter().map(|(p, t)| (p.to_string(), t.as_bytes())))
    }

    #[test]
    fn hash_is_stable() {
        // FNV-1a 64 test vector: a value change here breaks every stored file.
        assert_eq!(content_hash(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn file_binding_goes_stale_only_on_its_files() {
        let before = ws(&[("a.rs", "1"), ("b.rs", "1")]);
        let bound = before.bind(&["a.rs".to_string()]);
        assert!(matches!(bound, CodeState::Files(_)));
        let other_changed = ws(&[("a.rs", "1"), ("b.rs", "2")]);
        assert_eq!(other_changed.stale_reason(&bound), None);
        let changed = ws(&[("a.rs", "2"), ("b.rs", "1")]);
        assert_eq!(
            changed.stale_reason(&bound).as_deref(),
            Some("a.rs changed since")
        );
        let deleted = ws(&[("b.rs", "1")]);
        assert!(deleted.stale_reason(&bound).is_some());
    }

    #[test]
    fn tree_binding_goes_stale_on_any_change() {
        let before = ws(&[("a.rs", "1"), ("b.rs", "1")]);
        let bound = before.bind(&["missing.rs".to_string()]);
        assert_eq!(bound, CodeState::Tree(before.tree));
        assert_eq!(before.stale_reason(&bound), None);
        assert!(
            ws(&[("a.rs", "1"), ("b.rs", "2")])
                .stale_reason(&bound)
                .is_some()
        );
        assert!(
            ws(&[("a.rs", "1"), ("b.rs", "1"), ("c.rs", "")])
                .stale_reason(&bound)
                .is_some()
        );
    }
}

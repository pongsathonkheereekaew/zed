//! Task baseline: `path → Version` snapshot at task start (plan §16).
//!
//! Review compares `baseline → current`, never git HEAD — user manual changes
//! before task start are excluded by construction. Versions are headless `u64`
//! now, `clock::Global` under Zed (same trait shape).

use cedian_workspace::Version;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Baseline failures (fail closed — re-baseline, never guess).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaselineError {
    /// Path was never baselined (buffer opened after task start).
    NotBaselined { path: PathBuf },
}

impl std::fmt::Display for BaselineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotBaselined { path } => write!(f, "no baseline for {}", path.display()),
        }
    }
}

impl std::error::Error for BaselineError {}

/// Task-start version snapshot.
#[derive(Debug, Default, Clone)]
pub struct Baseline {
    versions: HashMap<PathBuf, Version>,
}

impl Baseline {
    /// Empty baseline.
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot one buffer's current version as its baseline.
    pub fn snapshot(&mut self, path: &Path, version: Version) {
        self.versions.insert(path.to_path_buf(), version);
    }

    /// Baseline version for a path, if recorded.
    pub fn version(&self, path: &Path) -> Result<Version, BaselineError> {
        self.versions
            .get(path)
            .copied()
            .ok_or_else(|| BaselineError::NotBaselined {
                path: path.to_path_buf(),
            })
    }

    /// Paths under review (baselined).
    pub fn paths(&self) -> Vec<PathBuf> {
        self.versions.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_and_lookup() {
        let mut b = Baseline::new();
        b.snapshot(Path::new("/a.rs"), Version(3));
        assert_eq!(b.version(Path::new("/a.rs")).unwrap(), Version(3));
        assert!(matches!(
            b.version(Path::new("/b.rs")),
            Err(BaselineError::NotBaselined { .. })
        ));
    }
}

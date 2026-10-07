//! Where cedian keeps a workspace's state (ADR-0044): outside the workspace,
//! so the implementer, which writes the workspace without a prompt, cannot
//! rewrite findings, evidence, the audit log or the correction ledger.
//!
//! Root: `$CEDIAN_STATE_DIR`, else `$XDG_STATE_HOME/cedian`, else
//! `~/.local/state/cedian`. Each workspace gets
//! `<root>/workspaces/<name>-<hash>/`, with a `workspace` file naming it.

use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};

fn root() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("CEDIAN_STATE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    // Unit tests never touch the user's real state.
    if cfg!(test) {
        return Ok(std::env::temp_dir().join(format!("cedian-test-state-{}", std::process::id())));
    }
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("cedian"));
    }
    let home = std::env::var_os("HOME").ok_or("HOME is not set, so cedian has no state dir")?;
    Ok(PathBuf::from(home).join(".local/state/cedian"))
}

/// The state directory for `workdir`, created on first use. Refused when it
/// resolves inside the workspace.
pub fn dir(workdir: &Path) -> Result<PathBuf, String> {
    dir_in(&root()?, workdir)
}

fn dir_in(root: &Path, workdir: &Path) -> Result<PathBuf, String> {
    let workdir = std::fs::canonicalize(workdir)
        .map_err(|e| format!("workspace {}: {e}", workdir.display()))?;
    let name: String = workdir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let digest = Sha256::digest(workdir.as_os_str().as_encoded_bytes());
    let hash: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    let dir = root.join("workspaces").join(format!("{name}-{hash}"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("state dir {}: {e}", dir.display()))?;
    let dir = std::fs::canonicalize(&dir).map_err(|e| format!("state dir: {e}"))?;
    if dir.starts_with(&workdir) {
        return Err(format!(
            "the state dir {} is inside the workspace, where the agent can write it; \
             set CEDIAN_STATE_DIR outside it",
            dir.display()
        ));
    }
    let marker = dir.join("workspace");
    if !marker.exists() {
        std::fs::write(&marker, format!("{}\n", workdir.display()))
            .map_err(|e| format!("state dir: {e}"))?;
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_dir_per_workspace_outside_it() {
        let base = std::env::temp_dir().join(format!("cedian-state-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for ws in ["a b", "c"] {
            std::fs::create_dir_all(base.join(ws)).unwrap();
        }
        let a = dir(&base.join("a b")).unwrap();
        let c = dir(&base.join("c")).unwrap();
        assert_ne!(a, c);
        assert_eq!(
            dir(&base.join("a b/../a b")).unwrap(),
            a,
            "same workspace, same dir"
        );
        assert!(a.file_name().unwrap().to_string_lossy().starts_with("ab-"));
        assert!(!a.starts_with(&base), "outside every workspace");
        let named = std::fs::read_to_string(a.join("workspace")).unwrap();
        assert!(named.trim_end().ends_with("a b"));
        let err = dir_in(&base.join("c/.state"), &base.join("c")).unwrap_err();
        assert!(err.contains("inside the workspace"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }
}

//! The CLI's view of the workspace state dir (ADR-0044); the rules live in
//! `cedian_shell::state`. Unit tests use a per-process temp root, never the
//! user's real state.

use std::path::{Path, PathBuf};

pub use cedian_shell::state::outside_workspace;

fn root() -> Result<PathBuf, String> {
    if cfg!(test) && std::env::var_os("CEDIAN_STATE_DIR").is_none() {
        return Ok(std::env::temp_dir().join(format!("cedian-test-state-{}", std::process::id())));
    }
    cedian_shell::state::root()
}

/// The state directory for `workdir`, created on first use.
pub fn dir(workdir: &Path) -> Result<PathBuf, String> {
    cedian_shell::state::dir_in(&root()?, workdir)
}

/// `name` inside the state directory for `workdir`.
pub fn file(workdir: &Path, name: &str) -> Result<PathBuf, String> {
    Ok(dir(workdir)?.join(name))
}

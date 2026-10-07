//! `shell.lock` in the state dir (ADR-0021 / P4, ADR-0044): one live `cedian shell` per workspace.
//!
//! The shell holds the lock (pid + start time, `snapshot_version`) for its
//! lifetime. Mutating one-shot commands check [`live_holder`] and refuse while
//! another live process holds it. A lock whose pid is dead is stale and is
//! reclaimed. Not a daemon: the lock dies with the shell (dropped or reclaimed).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Lock file schema version (P3 rule: every store is versioned).
pub const LOCK_SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct LockFile {
    #[serde(default)]
    snapshot_version: u32,
    pid: u32,
    started_ms: u64,
}

fn lock_path(workdir: &Path) -> Result<PathBuf, String> {
    Ok(crate::state::dir(workdir)?.join("shell.lock"))
}

/// Whether `pid` names a running process (`kill -0`; no `unsafe` needed).
fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn read(workdir: &Path) -> Option<LockFile> {
    let raw = std::fs::read_to_string(lock_path(workdir).ok()?).ok()?;
    serde_json::from_str(&raw).ok()
}

/// The pid of a LIVE shell holding this workspace, other than this process.
/// A corrupt or other-version lock counts as held only while its pid lives.
pub fn live_holder(workdir: &Path) -> Option<u32> {
    let lock = read(workdir)?;
    (lock.pid != std::process::id() && alive(lock.pid)).then_some(lock.pid)
}

/// Held for the shell's lifetime; removes the lock file on drop.
pub struct ShellLock {
    path: PathBuf,
}

impl ShellLock {
    /// Take the lock, reclaiming a stale one. Fails while a live shell holds it.
    pub fn acquire(workdir: &Path) -> Result<Self, String> {
        let path = lock_path(workdir)?;
        if let Some(pid) = live_holder(workdir) {
            return Err(format!(
                "a cedian shell (pid {pid}) already holds {} — use that shell",
                path.display()
            ));
        }
        let started_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let body = serde_json::to_string(&LockFile {
            snapshot_version: LOCK_SNAPSHOT_VERSION,
            pid: std::process::id(),
            started_ms,
        })
        .map_err(|e| e.to_string())?;
        // Atomic replace: a stale lock is overwritten in one step.
        let tmp = path.with_extension("lock.tmp");
        std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
        Ok(Self { path })
    }
}

impl Drop for ShellLock {
    fn drop(&mut self) {
        // Remove only our own lock (another shell may have reclaimed it).
        let ours = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| serde_json::from_str::<LockFile>(&raw).ok())
            .is_some_and(|l| l.pid == std::process::id());
        if ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Refuse a mutating one-shot command while a live shell holds the workspace.
pub fn refuse_if_shell_live(workdir: &Path, command: &str) -> Result<(), String> {
    match live_holder(workdir) {
        Some(pid) => Err(format!(
            "refused: a cedian shell (pid {pid}) holds this workspace — run `{command}` inside the shell"
        )),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cedian-lock-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_lock(d: &Path, pid: u32) {
        let body = format!(r#"{{"snapshot_version":1,"pid":{pid},"started_ms":1}}"#);
        std::fs::write(lock_path(d).unwrap(), body).unwrap();
    }

    #[test]
    fn acquire_writes_and_drop_removes() {
        let d = dir("own");
        let lock = ShellLock::acquire(&d).unwrap();
        assert!(lock_path(&d).unwrap().exists());
        // Our own lock never refuses our own one-shot calls.
        assert!(refuse_if_shell_live(&d, "accept").is_ok());
        drop(lock);
        assert!(!lock_path(&d).unwrap().exists());
    }

    #[test]
    fn live_foreign_holder_refuses() {
        let d = dir("live");
        // A process that is certainly alive and not us.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        write_lock(&d, child.id());
        assert_eq!(live_holder(&d), Some(child.id()));
        assert!(
            refuse_if_shell_live(&d, "accept")
                .unwrap_err()
                .contains("inside the shell")
        );
        assert!(ShellLock::acquire(&d).is_err());
        child.kill().unwrap();
        child.wait().unwrap();
        // Holder died → stale → reclaimed.
        assert_eq!(live_holder(&d), None);
        let _lock = ShellLock::acquire(&d).unwrap();
        assert_eq!(read(&d).unwrap().pid, std::process::id());
    }
}

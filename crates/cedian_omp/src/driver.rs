//! One driver per session (ADR-0040 decision 5). OMP lets a second process
//! resume a session another live process holds, and both then append to one
//! file (measured on OMP 18.6.1). cedian refuses instead: before it drives a
//! session it lists the processes holding the session's files, and any
//! process but its own OMP is another driver.
//!
//! The files: the session `.jsonl`, which a process keeps open for writing
//! once it has written to the session, and the owner lease OMP's creating
//! process holds an exclusive flock on, `<state root>/run/session-owners/
//! <sessionId>.lock` (the state root is `~/.omp`; `PI_CODING_AGENT_DIR` does
//! not move it). A process that resumed but has not written yet holds
//! neither, so it goes unseen.

use std::path::{Path, PathBuf};

/// The session's files another driver would hold.
pub fn session_files(
    state_root: &Path,
    session_id: &str,
    session_file: Option<&Path>,
) -> Vec<PathBuf> {
    let lease = state_root
        .join("run/session-owners")
        .join(format!("{session_id}.lock"));
    std::iter::once(lease)
        .chain(session_file.map(Path::to_path_buf))
        .collect()
}

/// OMP's state root: `~/.omp`.
pub fn state_root() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".omp"))
}

/// The holders that are not `own`, the app's OMP: other drivers.
pub fn other_drivers(holders: &[u32], own: u32) -> Vec<u32> {
    let mut others: Vec<u32> = holders.iter().copied().filter(|pid| *pid != own).collect();
    others.sort_unstable();
    others.dedup();
    others
}

/// Process ids holding any of `files` open, by `lsof`. Files that do not
/// exist are held by nobody.
pub fn holders(files: &[PathBuf]) -> Result<Vec<u32>, String> {
    let files: Vec<&PathBuf> = files.iter().filter(|f| f.exists()).collect();
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let lsof = ["/usr/sbin/lsof", "/usr/bin/lsof"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .unwrap_or("lsof");
    let out = std::process::Command::new(lsof)
        .arg("-t")
        .args(&files)
        .output()
        .map_err(|e| format!("cannot run lsof: {e}"))?;
    // lsof exits 1, silently, when no process has any of the files open.
    if !out.status.success() && out.stdout.is_empty() && !out.stderr.is_empty() {
        return Err(format!(
            "lsof failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect())
}

/// `Ok(pids)` of the other drivers of the session (empty when cedian may
/// drive it), or why it could not be checked.
pub fn other_drivers_of(files: &[PathBuf], own: u32) -> Result<Vec<u32>, String> {
    holders(files).map(|pids| other_drivers(&pids, own))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_apps_own_omp_is_not_another_driver() {
        assert_eq!(other_drivers(&[7, 9, 7, 3], 7), [3, 9]);
        assert!(other_drivers(&[7], 7).is_empty());
    }

    #[test]
    fn a_process_holding_the_session_file_is_another_driver() {
        let dir = std::env::temp_dir().join(format!("cedian-driver-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("s.jsonl");
        std::fs::write(&file, "").unwrap();
        let files = session_files(&dir, "s", Some(&file));
        assert!(
            other_drivers_of(&files, 1).unwrap().is_empty(),
            "nobody yet"
        );
        let mut holder = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("exec 3>>\"$0\"; exec sleep 30")
            .arg(&file)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let found = loop {
            let found = other_drivers_of(&files, 1).unwrap();
            if !found.is_empty() || std::time::Instant::now() > deadline {
                break found;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert_eq!(
            other_drivers_of(&files, holder.id()).unwrap(),
            Vec::<u32>::new(),
            "its own OMP"
        );
        holder.kill().ok();
        holder.wait().ok();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(found, [holder.id()]);
    }
}

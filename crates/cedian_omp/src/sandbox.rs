//! The reviewer's Seatbelt profile (ARCHITECTURE §64 mechanism 2, ADR-0039).
//!
//! Deny by default. A reviewer may read anything, reach the network, and
//! write only its own state: the session directory, the temp directories
//! and OMP's daemon run directory. The workspace is denied last, so a
//! workspace under `/private/tmp` stays unwritable. Process execution is
//! limited to OMP, the shells OMP's `bash` tool runs, and the allow-listed
//! executables: the kernel, not a policy check, refuses anything else.

use std::path::{Path, PathBuf};

pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
pub const REVIEWER_PROFILE_FILE: &str = "reviewer.sbpl";

/// Shells OMP's `bash` tool may start. A shell alone can only run builtins:
/// every other program needs its own `process-exec` rule.
const SHELLS: &[&str] = &["/bin/sh", "/bin/bash", "/bin/zsh"];

/// Inputs for one reviewer profile. Every path must be absolute.
#[derive(Debug, Clone)]
pub struct ReviewerSandbox {
    pub omp_binary: PathBuf,
    pub workspace: PathBuf,
    pub session_dir: PathBuf,
    /// OMP's daemon run directory (`~/.omp/run/daemons`).
    pub omp_run_dir: PathBuf,
    /// Allow-listed executables, already resolved to real paths.
    pub exec_allow: Vec<PathBuf>,
}

impl ReviewerSandbox {
    pub fn profile(&self) -> Result<String, String> {
        let lit = |p: &Path| -> Result<String, String> {
            let s = p.to_string_lossy();
            if !p.is_absolute() || s.chars().any(|c| c == '"' || c == '\\' || c.is_control()) {
                return Err(format!("sandbox path rejected: {s:?}"));
            }
            Ok(format!("\"{s}\""))
        };
        let mut exec = vec![format!("(literal {})", lit(&self.omp_binary)?)];
        for shell in SHELLS {
            exec.push(format!("(literal \"{shell}\")"));
        }
        for path in &self.exec_allow {
            exec.push(format!("(literal {})", lit(path)?));
        }
        Ok(format!(
            "(version 1)\n\
             (deny default)\n\
             (allow process-fork signal)\n\
             (allow process-exec {exec})\n\
             (allow sysctl-read mach-lookup ipc-posix-shm iokit-open user-preference-read)\n\
             (allow file-read*)\n\
             (allow network-outbound network-inbound system-socket)\n\
             (allow file-ioctl)\n\
             (allow file-write* (subpath {session}) (subpath {run}) (subpath \"/private/tmp\") \
             (subpath \"/private/var/folders\") (literal \"/dev/null\") (literal \"/dev/tty\") \
             (regex #\"^/dev/fd/\"))\n\
             (deny file-write* (subpath {workspace}))\n",
            exec = exec.join(" "),
            session = lit(&self.session_dir)?,
            run = lit(&self.omp_run_dir)?,
            workspace = lit(&self.workspace)?,
        ))
    }
}

/// Resolve each allow-list command's program (its first word) on `PATH` to
/// a real path. A command whose program is not found is skipped with a
/// note: it cannot run under the profile anyway.
pub fn resolve_allow_list(
    commands: &[String],
    path_var: Option<&str>,
) -> (Vec<PathBuf>, Vec<String>) {
    let mut found = Vec::new();
    let mut notes = Vec::new();
    for command in commands {
        let Some(program) = command.split_whitespace().next() else {
            continue;
        };
        match crate::resolve_on_path(program, path_var).and_then(|p| {
            std::fs::canonicalize(&p).map_err(|e| crate::OmpError::Spawn(e.to_string()))
        }) {
            Ok(real) if !found.contains(&real) => found.push(real),
            Ok(_) => {}
            Err(_) => notes.push(format!(
                "reviewer allow-list: {program:?} not found on PATH"
            )),
        }
    }
    (found, notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> ReviewerSandbox {
        ReviewerSandbox {
            omp_binary: "/opt/omp/bin/omp".into(),
            workspace: "/private/tmp/ws".into(),
            session_dir: "/Users/u/.cache/cedian/review-1".into(),
            omp_run_dir: "/Users/u/.omp/run/daemons".into(),
            exec_allow: vec!["/usr/bin/wc".into()],
        }
    }

    #[test]
    fn profile_denies_by_default_and_the_workspace_last() {
        let p = sandbox().profile().unwrap();
        assert!(p.starts_with("(version 1)\n(deny default)\n"));
        assert!(p.contains(
            "(allow process-exec (literal \"/opt/omp/bin/omp\") (literal \"/bin/sh\") \
             (literal \"/bin/bash\") (literal \"/bin/zsh\") (literal \"/usr/bin/wc\"))"
        ));
        assert!(p.contains("(subpath \"/Users/u/.cache/cedian/review-1\")"));
        assert!(
            p.trim_end()
                .ends_with("(deny file-write* (subpath \"/private/tmp/ws\"))"),
            "a workspace under /private/tmp stays unwritable: the deny comes last"
        );
    }

    #[test]
    fn quotes_and_relative_paths_are_refused() {
        let mut s = sandbox();
        s.workspace = "/tmp/a\")(allow file-write*".into();
        assert!(s.profile().is_err());
        let mut s = sandbox();
        s.exec_allow = vec!["wc".into()];
        assert!(s.profile().is_err());
    }

    #[test]
    fn allow_list_resolves_programs_and_notes_the_missing() {
        let (found, notes) = resolve_allow_list(
            &[
                "ls -la".to_string(),
                "nope-xyz --x".to_string(),
                "ls".to_string(),
            ],
            Some("/bin:/usr/bin"),
        );
        assert_eq!(
            found,
            [std::fs::canonicalize("/bin/ls").unwrap()],
            "one entry per program"
        );
        assert_eq!(notes.len(), 1);
    }
}

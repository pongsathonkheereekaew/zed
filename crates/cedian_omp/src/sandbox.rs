//! The reviewer's Seatbelt profile (ARCHITECTURE §64 mechanism 2, ADR-0041,
//! ADR-0043).
//!
//! Deny by default. A reviewer may read anything except credentials, reach
//! the network, and write only its run directory, which cedian creates fresh
//! for each review: OMP's session, `TMPDIR`, and OMP's state and cache roots
//! all point inside it. The overlay and this profile sit beside the run
//! directory, out of the reviewer's reach. The workspace is denied last.
//! Process execution is limited to OMP, the shells OMP's `bash` tool runs,
//! and the allow-listed executables: the kernel, not a policy check, refuses
//! anything else.

use std::path::{Path, PathBuf};

pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
pub const REVIEWER_PROFILE_FILE: &str = "reviewer.sbpl";

/// Shells OMP's `bash` tool may start. Builtins and redirections still work
/// in them, so every limit on what a shell reaches is a file or exec rule.
const SHELLS: &[&str] = &["/bin/sh", "/bin/bash", "/bin/zsh"];

/// Credential locations under `HOME` the reviewer may not read (ADR-0043).
/// OMP's own auth store (`~/.omp/agent`) is not here: the reviewer needs it.
const CREDENTIAL_DIRS: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".config/gh",
    ".config/gcloud",
    ".azure",
    ".kube",
    ".docker",
    ".password-store",
    "Library/Keychains",
];
const CREDENTIAL_FILES: &[&str] = &[".netrc", ".git-credentials", ".npmrc", ".pypirc"];

/// Where one reviewer's files live. `dir` belongs to cedian; only `run/` is
/// the reviewer's, and [`Self::reset`] recreates it for every review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewerLayout {
    pub dir: PathBuf,
}

impl ReviewerLayout {
    pub fn profile(&self) -> PathBuf {
        self.dir.join(REVIEWER_PROFILE_FILE)
    }

    /// The one directory the reviewer may write.
    pub fn run(&self) -> PathBuf {
        self.dir.join("run")
    }

    /// OMP's `--session-dir`.
    pub fn session(&self) -> PathBuf {
        self.run().join("session")
    }

    /// `TMPDIR`, `XDG_STATE_HOME` and `XDG_CACHE_HOME` for the reviewer. OMP
    /// keeps its run, log and state files under `$XDG_STATE_HOME/omp` and its
    /// cache under `$XDG_CACHE_HOME/omp` when those directories exist.
    pub fn env(&self) -> Vec<(String, String)> {
        let run = self.run();
        [
            ("TMPDIR", "tmp"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
        ]
        .into_iter()
        .map(|(name, sub)| {
            (
                name.to_string(),
                run.join(sub).to_string_lossy().into_owned(),
            )
        })
        .collect()
    }

    /// Delete the last review's run directory and create an empty one.
    pub fn reset(&self) -> std::io::Result<()> {
        let run = self.run();
        match std::fs::remove_dir_all(&run) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        for sub in ["session", "tmp", "state/omp", "cache/omp"] {
            std::fs::create_dir_all(run.join(sub))?;
        }
        Ok(())
    }
}

/// Inputs for one reviewer profile. Every path must be absolute and
/// canonical: Seatbelt matches resolved paths, so `/tmp/x` would never match
/// a write to `/private/tmp/x`.
#[derive(Debug, Clone)]
pub struct ReviewerSandbox {
    pub omp_binary: PathBuf,
    pub workspace: PathBuf,
    /// [`ReviewerLayout::run`]: the only writable directory.
    pub run_dir: PathBuf,
    /// Paths the reviewer may not read; see [`credential_paths`].
    pub read_deny: Vec<PathBuf>,
    /// Allow-listed executables, already resolved to real paths.
    pub exec_allow: Vec<PathBuf>,
}

/// The credential paths under `home`. An existing path that resolves
/// elsewhere (a symlinked `~/.ssh`) is listed under both names.
pub fn credential_paths(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for rel in CREDENTIAL_DIRS.iter().chain(CREDENTIAL_FILES) {
        let path = home.join(rel);
        if let Ok(real) = std::fs::canonicalize(&path) {
            if real != path {
                out.push(real);
            }
        }
        out.push(path);
    }
    out
}

impl ReviewerSandbox {
    pub fn profile(&self) -> Result<String, String> {
        let lit = |p: &Path| -> Result<String, String> {
            let Some(s) = p.to_str() else {
                return Err(format!("sandbox path is not UTF-8: {}", p.display()));
            };
            if !p.is_absolute() || s.chars().any(|c| c == '"' || c == '\\' || c.is_control()) {
                return Err(format!("sandbox path rejected: {s:?}"));
            }
            Ok(format!("\"{s}\""))
        };
        let canonical = |label: &str, p: &Path| -> Result<String, String> {
            if std::fs::canonicalize(p).ok().as_deref() != Some(p) {
                return Err(format!("{label} is not a canonical path: {}", p.display()));
            }
            lit(p)
        };
        let run = canonical("run dir", &self.run_dir)?;
        let workspace = canonical("workspace", &self.workspace)?;
        // A program the reviewer or the implementer can rewrite is not an
        // allow-list entry: it would run whatever they put there.
        let executable = |p: &Path| -> Result<String, String> {
            let s = canonical("executable", p)?;
            for root in [&self.run_dir, &self.workspace] {
                if p.starts_with(root) {
                    return Err(format!(
                        "executable {} is under {}, which the agents can write",
                        p.display(),
                        root.display()
                    ));
                }
            }
            Ok(format!("(literal {s})"))
        };
        let mut exec = vec![executable(&self.omp_binary)?];
        for shell in SHELLS {
            exec.push(format!("(literal \"{shell}\")"));
        }
        for path in &self.exec_allow {
            exec.push(executable(path)?);
        }
        let read_deny = self
            .read_deny
            .iter()
            .map(|p| Ok(format!("(subpath {})", lit(p)?)))
            .collect::<Result<Vec<_>, String>>()?;
        let read_deny = if read_deny.is_empty() {
            String::new()
        } else {
            format!("(deny file-read* {})\n", read_deny.join(" "))
        };
        Ok(format!(
            "(version 1)\n\
             (deny default)\n\
             (allow process-fork signal)\n\
             (allow process-exec {exec})\n\
             (allow sysctl-read mach-lookup ipc-posix-shm iokit-open user-preference-read)\n\
             (allow file-read*)\n\
             {read_deny}\
             (allow network-outbound network-inbound system-socket)\n\
             (allow file-ioctl)\n\
             (allow file-write* (subpath {run}) (literal \"/dev/null\") (literal \"/dev/tty\") \
             (regex #\"^/dev/fd/\"))\n\
             (deny file-write* (subpath {workspace}))\n",
            exec = exec.join(" "),
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

    struct Dirs {
        root: PathBuf,
        sandbox: ReviewerSandbox,
    }

    impl Drop for Dirs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn dirs(tag: &str) -> Dirs {
        let root = std::env::temp_dir().join(format!("cedian-sbpl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for sub in ["ws", "reviewer/run"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let root = std::fs::canonicalize(&root).unwrap();
        let sandbox = ReviewerSandbox {
            omp_binary: std::fs::canonicalize("/bin/ls").unwrap(),
            workspace: root.join("ws"),
            run_dir: root.join("reviewer/run"),
            read_deny: vec!["/Users/u/.ssh".into()],
            exec_allow: vec![std::fs::canonicalize("/usr/bin/wc").unwrap()],
        };
        Dirs { root, sandbox }
    }

    #[test]
    fn profile_writes_only_the_run_dir_and_denies_the_workspace_last() {
        let d = dirs("shape");
        let p = d.sandbox.profile().unwrap();
        assert!(p.starts_with("(version 1)\n(deny default)\n"));
        assert!(p.contains("(literal \"/bin/sh\")"));
        assert!(p.contains("(deny file-read* (subpath \"/Users/u/.ssh\"))"));
        let write = p
            .lines()
            .find(|l| l.starts_with("(allow file-write*"))
            .unwrap();
        assert!(write.contains(&format!(
            "(subpath \"{}\")",
            d.root.join("reviewer/run").display()
        )));
        assert!(!write.contains("/private/tmp\"") && !write.contains("/private/var/folders\""));
        assert!(
            p.trim_end().ends_with(&format!(
                "(deny file-write* (subpath \"{}\"))",
                d.root.join("ws").display()
            )),
            "the workspace deny comes last"
        );
    }

    #[test]
    fn quotes_relative_and_non_canonical_paths_are_refused() {
        let d = dirs("refuse");
        let mut s = d.sandbox.clone();
        s.workspace = "/tmp/a\")(allow file-write*".into();
        assert!(s.profile().is_err());
        let mut s = d.sandbox.clone();
        s.exec_allow = vec!["wc".into()];
        assert!(s.profile().is_err());
        let mut s = d.sandbox.clone();
        s.workspace = d.root.join("ws/../ws");
        assert!(
            s.profile().is_err(),
            "a `..` path never matches what Seatbelt sees"
        );
    }

    #[test]
    fn executables_the_agents_can_write_are_refused() {
        let d = dirs("exec");
        for dir in ["ws", "reviewer/run"] {
            let tool = d.root.join(dir).join("tool");
            std::fs::write(&tool, "").unwrap();
            let mut s = d.sandbox.clone();
            s.exec_allow = vec![tool.clone()];
            assert!(
                s.profile()
                    .unwrap_err()
                    .contains("which the agents can write")
            );
            let mut s = d.sandbox.clone();
            s.omp_binary = tool;
            assert!(s.profile().is_err());
        }
    }

    #[test]
    fn layout_reset_empties_the_run_dir_and_keeps_the_rest() {
        let d = dirs("layout");
        let layout = ReviewerLayout {
            dir: d.root.join("reviewer"),
        };
        std::fs::write(layout.run().join("planted"), "x").unwrap();
        std::fs::write(layout.profile(), "keep").unwrap();
        layout.reset().unwrap();
        assert!(!layout.run().join("planted").exists());
        assert!(layout.run().join("state/omp").is_dir() && layout.session().is_dir());
        assert_eq!(std::fs::read_to_string(layout.profile()).unwrap(), "keep");
        let env = layout.env();
        assert!(
            env.iter()
                .any(|(k, v)| k == "TMPDIR" && v.ends_with("/run/tmp"))
        );
    }

    #[test]
    fn credential_paths_cover_both_names_of_a_symlink() {
        let d = dirs("creds");
        let home = d.root.join("home");
        std::fs::create_dir_all(d.root.join("dotfiles/ssh")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::os::unix::fs::symlink(d.root.join("dotfiles/ssh"), home.join(".ssh")).unwrap();
        let paths = credential_paths(&home);
        assert!(paths.contains(&home.join(".ssh")));
        assert!(paths.contains(&d.root.join("dotfiles/ssh")));
        assert!(paths.contains(&home.join(".netrc")));
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

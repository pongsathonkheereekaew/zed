//! S3 gate item 2: the reviewer profile is enforced by the kernel. Each case
//! runs a real `sandbox-exec` with the generated profile; nothing here asks
//! a policy check. The workspace sits under the temp directory, which the
//! profile otherwise lets a reviewer write, so the workspace deny must win.
#![cfg(target_os = "macos")]
#![allow(
    clippy::disallowed_methods,
    reason = "headless, synchronous process control (OMP, LSP, DAP, Chrome, git, sandbox-exec): Zed's async spawn helpers do not apply"
)]

use cedian_omp::sandbox::{ReviewerLayout, ReviewerSandbox, SANDBOX_EXEC, credential_paths};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    root: PathBuf,
    ws: PathBuf,
    layout: ReviewerLayout,
}

impl Fixture {
    fn sandbox(&self, omp_binary: PathBuf, exec_allow: Vec<PathBuf>) -> ReviewerSandbox {
        ReviewerSandbox {
            omp_binary,
            workspace: self.ws.clone(),
            run_dir: self.layout.run(),
            read_deny: credential_paths(&self.root.join("home")),
            exec_allow,
        }
    }
}

fn fixture(tag: &str) -> Fixture {
    let root = std::env::temp_dir()
        .join(format!("cedian-sbx-{tag}-{}", std::process::id()))
        .canonicalize_or_create();
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("notes.txt"), "hello\n").unwrap();
    let layout = ReviewerLayout {
        dir: root.join("reviewer"),
    };
    layout.reset().unwrap();
    let f = Fixture { root, ws, layout };
    let text = f
        .sandbox(
            PathBuf::from("/usr/bin/true"),
            vec![std::fs::canonicalize("/bin/ls").unwrap()],
        )
        .profile()
        .unwrap();
    std::fs::write(f.layout.profile(), text).unwrap();
    f
}

trait CanonOrCreate {
    fn canonicalize_or_create(self) -> PathBuf;
}

impl CanonOrCreate for PathBuf {
    fn canonicalize_or_create(self) -> PathBuf {
        let _ = std::fs::remove_dir_all(&self);
        std::fs::create_dir_all(&self).unwrap();
        self.canonicalize().unwrap()
    }
}

fn sandboxed(f: &Fixture, script: &str) -> Output {
    Command::new(SANDBOX_EXEC)
        .arg("-f")
        .arg(f.layout.profile())
        .args(["/bin/sh", "-c", script])
        .current_dir(&f.ws)
        .output()
        .unwrap()
}

fn denied(out: &Output) -> bool {
    !out.status.success()
        && String::from_utf8_lossy(&out.stderr).contains("Operation not permitted")
}

fn cleanup(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn workspace_write_is_denied_by_the_kernel() {
    let f = fixture("write");
    let target = f.ws.join("notes.txt");
    let out = sandboxed(&f, &format!("echo pwned >> '{}'", target.display()));
    assert!(denied(&out), "append refused: {out:?}");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "hello\n",
        "file unchanged"
    );
    let out = sandboxed(
        &f,
        &format!("echo x > '{}'", f.ws.join("new.txt").display()),
    );
    assert!(denied(&out), "create refused: {out:?}");
    assert!(!f.ws.join("new.txt").exists());
    cleanup(&f.root);
}

#[test]
fn exec_outside_the_allow_list_is_denied_by_the_kernel() {
    let f = fixture("exec");
    let probe = f.layout.run().join("touched");
    let out = sandboxed(&f, &format!("/usr/bin/touch '{}'", probe.display()));
    assert!(denied(&out), "touch is not allow-listed: {out:?}");
    assert!(
        !probe.exists(),
        "and it never ran, though the run dir is writable"
    );
    cleanup(&f.root);
}

#[test]
fn allow_listed_command_and_run_dir_writes_still_work() {
    let f = fixture("allow");
    let out = sandboxed(&f, "/bin/ls");
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("notes.txt"));
    let out = sandboxed(
        &f,
        &format!("echo ok > '{}'", f.layout.run().join("state/x").display()),
    );
    assert!(
        out.status.success(),
        "the reviewer's own state is writable: {out:?}"
    );
    cleanup(&f.root);
}

#[test]
fn without_the_profile_the_same_commands_succeed() {
    let f = fixture("control");
    let probe = f.layout.run().join("touched");
    let ok = Command::new("/usr/bin/touch").arg(&probe).status().unwrap();
    assert!(
        ok.success() && probe.exists(),
        "the deny above is the sandbox, not the setup"
    );
    cleanup(&f.root);
}

/// An OMP turn completes under the reviewer profile (needs a real `omp`).
#[test]
#[ignore]
fn omp_turn_completes_under_the_reviewer_profile() {
    let f = fixture("omp");
    let omp = cedian_omp::resolve_on_path("omp", std::env::var("PATH").ok().as_deref()).unwrap();
    let omp = std::fs::canonicalize(omp).unwrap();
    let home = PathBuf::from(std::env::var("HOME").unwrap());
    let mut sandbox = f.sandbox(omp.clone(), Vec::new());
    sandbox.read_deny = credential_paths(&home);
    std::fs::write(f.layout.profile(), sandbox.profile().unwrap()).unwrap();
    let out = Command::new(SANDBOX_EXEC)
        .arg("-f")
        .arg(f.layout.profile())
        .arg(&omp)
        .arg("--session-dir")
        .arg(f.layout.session())
        .args(["-p", "Reply with exactly: sbx-ok"])
        .current_dir(&f.ws)
        .env_clear()
        .envs(cedian_omp::scrub_env(std::env::vars()))
        .envs(f.layout.env())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("sbx-ok"),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        f.layout.run().join("state/omp/run").is_dir(),
        "OMP kept its run state in the reviewer's own directory"
    );
    cleanup(&f.root);
}

#[test]
fn writes_outside_the_run_dir_are_denied_by_the_kernel() {
    let f = fixture("roots");
    let elsewhere = f.root.join("elsewhere");
    let out = sandboxed(&f, &format!("echo x > '{}'", elsewhere.display()));
    assert!(denied(&out), "a temp root is not the reviewer's: {out:?}");
    assert!(!elsewhere.exists());
    for cedian_owned in [f.layout.profile(), f.layout.dir.join("cedian-overlay.yml")] {
        let out = sandboxed(&f, &format!("echo x > '{}'", cedian_owned.display()));
        assert!(
            denied(&out),
            "{} is cedian's: {out:?}",
            cedian_owned.display()
        );
    }
    cleanup(&f.root);
}

#[test]
fn credential_reads_are_denied_by_the_kernel() {
    let f = fixture("creds");
    let key = f.root.join("home/.ssh/id_test");
    std::fs::create_dir_all(key.parent().unwrap()).unwrap();
    std::fs::write(&key, "secret\n").unwrap();
    let out = sandboxed(
        &f,
        &format!("read line < '{}' && echo \"$line\"", key.display()),
    );
    assert!(denied(&out), "the key stays unread: {out:?}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("secret"));
    cleanup(&f.root);
}

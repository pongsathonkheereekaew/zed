//! B3 hidden test: removing a worker never destroys unmerged work.
use cedian_worker::worktree;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git").args(args).current_dir(dir).status().unwrap().success();
    assert!(ok, "git {args:?}");
}

fn repo(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bench-b3-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.email", "b@b"]);
    git(&dir, &["config", "user.name", "b"]);
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "init"]);
    dir.canonicalize().unwrap()
}

fn branch_exists(dir: &Path, branch: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", branch])
        .current_dir(dir)
        .status()
        .unwrap()
        .success()
}

#[test]
fn unmerged_worker_is_kept() {
    let dir = repo("unmerged");
    let head = worktree::spawn(&dir, "w1", "main").unwrap();
    let wt = dir.join(&head.worktree);
    std::fs::write(wt.join("b.txt"), "b\n").unwrap();
    git(&wt, &["add", "."]);
    git(&wt, &["commit", "-qm", "work"]);
    assert!(worktree::remove(&dir, &head).is_err(), "unmerged remove must fail");
    assert!(branch_exists(&dir, &head.branch), "branch and its commit kept");
}

#[test]
fn merged_worker_is_removed() {
    let dir = repo("merged");
    let head = worktree::spawn(&dir, "w2", "main").unwrap();
    worktree::remove(&dir, &head).unwrap();
    assert!(!branch_exists(&dir, &head.branch));
}

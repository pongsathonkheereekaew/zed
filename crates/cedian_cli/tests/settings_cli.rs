//! Row E through the real CLI binary: the user's `cedian.toml` is the only
//! settings file. Hermetic: every run gets its own `CEDIAN_CONFIG`.
#![allow(
    clippy::disallowed_methods,
    reason = "headless, synchronous process control (OMP, LSP, DAP, Chrome, git, sandbox-exec): Zed's async spawn helpers do not apply"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Root(PathBuf);

impl Root {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("cedian-rowe-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ws")).unwrap();
        Self(dir.canonicalize().unwrap())
    }

    fn config(&self, toml: &str) -> PathBuf {
        let path = self.0.join("cedian.toml");
        std::fs::write(&path, toml).unwrap();
        path
    }

    fn run(&self, config: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cedian"))
            .args(args)
            .env("CEDIAN_CONFIG", config)
            .env("CEDIAN_WORKDIR", self.0.join("ws"))
            .env("CEDIAN_SESSION_DIR", self.0.join("sessions"))
            .env("CEDIAN_STATE_DIR", self.0.join("state"))
            .env("CEDIAN_OMP_BINARY", "/nonexistent/omp")
            .env("HOME", self.0.join("home"))
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap()
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn floor_from_cedian_toml_joins_a_started_workflow() {
    let root = Root::new("floor");
    let config = root.config(
        "schema = 1\n[[workflow.floor]]\nkind = \"bug_fix\"\nmin_risk = \"low\"\n\
         gates = [{ id = \"floor-tests\", gate_kind = \"test\", evidence_kinds = [\"test\"] }]\n",
    );
    let out = root.run(&config, &["workflow", "run", "bug_fix", "fix it"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("gate floor-tests: Pending"),
        "{}",
        text(&out)
    );

    let fast = Root::new("no-floor");
    let out = fast.run(
        &fast.config("schema = 1\n"),
        &["workflow", "run", "bug_fix", "fix it"],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(!text(&out).contains("floor-tests"), "{}", text(&out));
}

#[test]
fn leftover_cedian_json_is_refused_naming_the_move() {
    let root = Root::new("legacy");
    let config = root.config("schema = 1\n");
    std::fs::write(root.0.join("ws/cedian.json"), "{\"schema\": 1}").unwrap();
    let out = root.run(&config, &["review"]);
    assert!(!out.status.success());
    let err = text(&out);
    assert!(
        err.contains("cedian.json is no longer read")
            && err.contains(&config.display().to_string())
            && err.contains("schema = 1"),
        "{err}"
    );
}

#[test]
fn permissions_still_gate_prompt() {
    let root = Root::new("deny");
    let config = root.config(
        "schema = 1\n[permissions]\nsafe = \"allow\"\nproject_write = \"allow\"\ndangerous = \"deny\"\n",
    );
    let out = root.run(&config, &["prompt", "hi"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("refused: settings [permissions] dangerous = deny"),
        "{}",
        text(&out)
    );
}

#[test]
fn bad_or_missing_user_file_fails_closed() {
    let root = Root::new("bad");
    let out = root.run(&root.config("schema = 1\nyolo = true\n"), &["review"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("unknown field `yolo`"),
        "{}",
        text(&out)
    );
    let out = root.run(&root.0.join("typo.toml"), &["review"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("does not exist"), "{}", text(&out));
}

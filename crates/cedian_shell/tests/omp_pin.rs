//! ADR-0057 decision 4: cedian runs the pinned OMP when one is found (the
//! `omp` on PATH, then `~/.local/bin/omp`, then `CEDIAN_OMP_BINARY`, each
//! asked `--version`), else the first one found with a warning naming both
//! versions. One test: it sets the process environment.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

fn fake(dir: &Path, version: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("omp");
    std::fs::write(&path, format!("#!/bin/sh\necho omp/{version}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn the_pinned_omp_is_found_first_else_the_first_one_warns() {
    let root = std::env::temp_dir().join(format!("cedian-omp-pin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let on_path = fake(&root.join("bin"), "18.7.0");
    let home = root.join("home");
    let local = fake(&home.join(".local/bin"), "18.6.1");
    let env_omp = fake(&root.join("env"), "18.6.1");
    let set = |path: &Path, home: &Path, env: Option<&Path>| unsafe {
        // SAFETY: the only test in this binary; nothing else reads the env.
        std::env::set_var("PATH", path);
        std::env::set_var("HOME", home);
        match env {
            Some(env) => std::env::set_var("CEDIAN_OMP_BINARY", env),
            None => std::env::remove_var("CEDIAN_OMP_BINARY"),
        }
    };

    set(&root.join("bin"), &home, Some(&env_omp));
    let choice = cedian_shell::launch::omp_binary().unwrap();
    assert_eq!(
        choice.binary, local,
        "PATH's 18.7.0 is skipped for ~/.local/bin's pin"
    );
    assert_eq!(choice.warning, None);

    let nobody = root.join("nobody");
    set(&root.join("bin"), &nobody, Some(&env_omp));
    assert_eq!(cedian_shell::launch::omp_binary().unwrap().binary, env_omp);

    set(&root.join("bin"), &nobody, None);
    let choice = cedian_shell::launch::omp_binary().unwrap();
    assert_eq!(choice.binary, on_path, "none pinned: the first found runs");
    let warning = choice.warning.expect("a warning when no omp is the pin");
    assert!(
        warning.contains("18.7.0") && warning.contains("18.6.1"),
        "{warning}"
    );

    set(&root.join("empty"), &nobody, None);
    assert!(
        cedian_shell::launch::omp_binary().is_err(),
        "no omp anywhere"
    );
    let _ = std::fs::remove_dir_all(&root);
}

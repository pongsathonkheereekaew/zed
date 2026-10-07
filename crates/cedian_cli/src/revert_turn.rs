//! `cedian turns` + `cedian revert-turn <n|last>` (P6, ADR-0026 decision 3).
//!
//! One action puts back everything turn `n` changed, file by file, through
//! the 3-way [`cedian_review::revert_file`]: regions changed since are
//! `STALE`, listed, and left alone. The revert is recorded as its own turn
//! (so `revert-turn` on it redoes the original) and as `AgentEdit`s, so the
//! review tracker's latest agent text follows the revert.

use crate::session::{self, TurnFile, TurnKind, TurnRecord};
use crate::workspace_files;
use cedian_review::AgentEdit;
use std::path::Path;

/// List recorded turns, oldest first.
pub fn cmd_turns(workdir: &Path) -> Result<(), String> {
    let store = session::load(workdir)?.unwrap_or_default();
    if store.turns.is_empty() {
        println!("no turns recorded");
    }
    for turn in &store.turns {
        println!("{}", describe(turn));
    }
    Ok(())
}

fn describe(turn: &TurnRecord) -> String {
    let kind = match &turn.kind {
        TurnKind::Prompt => "prompt".to_string(),
        TurnKind::Edit => "edit".to_string(),
        TurnKind::Revert { of } => format!("revert of {of}"),
    };
    let files: Vec<&str> = turn
        .files
        .iter()
        .map(|f| f.file.trim_start_matches('/'))
        .collect();
    format!("{} [{kind}] {} — {}", turn.n, turn.label, files.join(", "))
}

/// Revert turn `which` (`last` or a number).
pub fn cmd_revert_turn(workdir: &Path, which: &str) -> Result<(), String> {
    let mut store = session::load(workdir)?.ok_or("no turns recorded")?;
    let turn = match which {
        "last" => store.turns.last(),
        n => {
            let n: u32 = n
                .parse()
                .map_err(|_| "usage: cedian revert-turn <n|last>")?;
            store.turns.iter().find(|t| t.n == n)
        }
    }
    .cloned()
    .ok_or_else(|| format!("no turn {which:?} (see `cedian turns`)"))?;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut changed = Vec::new();
    let mut stale_total = 0;
    for file in &turn.files {
        let local = workspace_files::local_path(workdir, Path::new(&file.file))
            .ok_or_else(|| format!("path outside the workspace: {}", file.file))?;
        let existed = local.exists();
        let current = std::fs::read_to_string(&local).unwrap_or_default();
        let result = cedian_review::revert_file(&file.before, &file.after, &current);
        let rel = file.file.trim_start_matches('/');
        for hunk in &result.stale {
            stale_total += 1;
            println!(
                "  STALE {rel}: turn lines {}-{} changed since — kept as is",
                hunk.after_start + 1,
                hunk.after_start + hunk.after_count.max(1)
            );
        }
        // A file the turn created goes away again only when nothing in it
        // was kept.
        let delete = file.created && result.text.is_empty() && result.stale.is_empty();
        if delete {
            if existed {
                std::fs::remove_file(&local).map_err(|e| e.to_string())?;
            }
        } else if result.text != current || !existed {
            if let Some(dir) = local.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            std::fs::write(&local, &result.text).map_err(|e| e.to_string())?;
        }
        if result.reverted == 0 {
            continue;
        }
        println!("  {rel}: {} hunk(s) put back", result.reverted);
        let key = Path::new(&file.file);
        store.baseline_once(key, &current);
        store.record(AgentEdit {
            tool_call_id: format!("revert-{}", turn.n),
            task_id: session::CLI_TASK.to_string(),
            file: file.file.clone(),
            before: current.clone(),
            after: result.text.clone(),
            timestamp_ms: now_ms,
        });
        changed.push(TurnFile {
            file: file.file.clone(),
            before: current,
            after: result.text,
            created: !existed,
        });
    }
    for file in &turn.files {
        crate::corrections::record(
            workdir,
            crate::corrections::CorrectionKind::TurnReverted,
            crate::corrections::Event {
                turn: Some(turn.n),
                path: Some(file.file.clone()),
                excerpt: Some(file.after.clone()),
                ..crate::corrections::Event::default()
            },
        )?;
    }
    let label = format!("revert turn {}", turn.n);
    match store.record_turn(TurnKind::Revert { of: turn.n }, &label, changed) {
        Some(n) => println!(
            "reverted turn {} as turn {n} ({stale_total} STALE kept) — `revert-turn {n}` redoes it",
            turn.n
        ),
        None => println!(
            "turn {}: nothing to put back ({stale_total} STALE kept)",
            turn.n
        ),
    }
    session::save(workdir, &store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ReviewStore;

    fn workdir(tag: &str) -> crate::test_dir::TestDir {
        crate::test_dir::TestDir::new(&format!("revert-{tag}"))
    }

    fn file(key: &str, before: &str, after: &str, created: bool) -> TurnFile {
        TurnFile {
            file: key.into(),
            before: before.into(),
            after: after.into(),
            created,
        }
    }

    #[test]
    fn revert_skips_stale_drops_created_and_redo_restores() {
        let dir = workdir("cli");
        // Turn 1 changed a.txt lines 1 and 3 and created new.txt; since then
        // the user rewrote line 3.
        std::fs::write(dir.join("a.txt"), "ONE\ntwo\nmine\n").unwrap();
        std::fs::write(dir.join("new.txt"), "fresh\n").unwrap();
        let mut store = ReviewStore::new();
        store.record_turn(
            TurnKind::Prompt,
            "shout",
            vec![
                file("/a.txt", "one\ntwo\nthree\n", "ONE\ntwo\nTHREE\n", false),
                file("/new.txt", "", "fresh\n", true),
            ],
        );
        session::save(&dir, &store).unwrap();

        cmd_revert_turn(&dir, "last").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "one\ntwo\nmine\n",
            "line 1 back, the user's line 3 kept"
        );
        assert!(!dir.join("new.txt").exists(), "created file removed");
        let store = session::load(&dir).unwrap().unwrap();
        assert_eq!(store.turns[1].kind, TurnKind::Revert { of: 1 });
        assert!(
            store
                .provenance
                .iter()
                .all(|e| e.tool_call_id == "revert-1")
        );

        cmd_revert_turn(&dir, "2").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "ONE\ntwo\nmine\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("new.txt")).unwrap(),
            "fresh\n"
        );
        assert_eq!(
            session::load(&dir).unwrap().unwrap().turns[2].kind,
            TurnKind::Revert { of: 2 }
        );
        assert!(cmd_revert_turn(&dir, "9").unwrap_err().contains("no turn"));
    }
}

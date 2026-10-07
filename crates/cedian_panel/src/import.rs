//! Import an OMP-native disk write as ONE agent-attributed buffer transaction
//! (cedian ARCHITECTURE §12, ADR-0027, S9a T4).
//!
//! OMP's `edit`/`write` write the filesystem; Zed would otherwise reload the
//! file silently. At the tool's start [`begin`] seals the buffer's history so
//! the agent edit never groups with the user's typing. At its end [`finish`]
//! reloads the buffer itself (not waiting for the watcher), folds a watcher
//! reload that raced ahead into the same transaction, and returns the one
//! transaction id the caller keys by `tool_call_id`. Native undo reverts it.
//!
//! Never overwrites unsaved user edits: a dirty buffer is `Stale` and is left
//! untouched (`Buffer::reload` itself does not check dirtiness).

use anyhow::Result;
use gpui::{AsyncApp, Entity};
use language::Buffer;
use text::TransactionId;

/// Buffer state captured when an edit-class tool starts.
#[derive(Clone)]
pub struct Mark {
    start: text::BufferSnapshot,
    start_top: Option<TransactionId>,
}

impl Mark {
    /// The buffer as it was when the tool started: a file's review baseline
    /// on the task's first edit to it.
    pub fn start(&self) -> &text::BufferSnapshot {
        &self.start
    }
}

/// What [`finish`] did to one buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportOutcome {
    /// The tool changed this file: one transaction, attributable to the call.
    Imported(TransactionId),
    /// The file did not change during the tool call.
    Unchanged,
    /// The user has unsaved edits: nothing was reloaded (review shows STALE).
    Stale,
}

fn top(buffer: &Buffer) -> Option<TransactionId> {
    buffer.peek_undo_stack().map(|entry| entry.transaction_id())
}

/// Seal the history and remember where the tool call started.
pub fn begin(buffer: &mut Buffer) -> Mark {
    buffer.finalize_last_transaction();
    Mark {
        start: buffer.text_snapshot(),
        start_top: top(buffer),
    }
}

/// Load the disk text and fold everything since [`begin`] into one
/// transaction. The import applies the disk diff itself rather than calling
/// `Buffer::reload`: the project's watcher reloads the same change on its
/// own, and a second `reload` cancels the first, so the import would see
/// nothing while the watcher's reload landed after it. Applying the diff
/// directly is safe in either order: whichever lands second finds its hunks
/// already in the buffer and applies nothing.
pub async fn finish(
    buffer: Entity<Buffer>,
    mark: Mark,
    cx: &mut AsyncApp,
) -> Result<ImportOutcome> {
    let (dirty, watcher_txn, load) = buffer.read_with(cx, |b, cx| {
        let raced = (top(b) != mark.start_top).then(|| top(b)).flatten();
        let load = b.file().and_then(|f| f.as_local()).map(|f| f.load(cx));
        (b.is_dirty(), raced, load)
    });
    if dirty {
        return Ok(ImportOutcome::Stale);
    }
    let Some(load) = load else {
        return Ok(ImportOutcome::Unchanged);
    };
    let new_text = load.await?;
    let diff = buffer.read_with(cx, |b, cx| b.diff(new_text, cx)).await;

    buffer.update(cx, |b, cx| {
        b.finalize_last_transaction();
        let ours = b.apply_diff(diff, cx);
        b.finalize_last_transaction();
        let changed = b.version() != *mark.start.version();
        if changed {
            let mtime = b.file().and_then(|f| f.disk_state().mtime());
            b.did_reload(b.version(), b.line_ending(), mtime, cx);
        }
        let id = match (watcher_txn, ours) {
            (Some(watcher), Some(ours)) if watcher != ours => {
                b.merge_transactions(ours, watcher);
                Some(watcher)
            }
            // The watcher may have landed between the check above and the
            // apply: the top of the undo stack is then its transaction.
            _ => top(b).filter(|t| Some(*t) != mark.start_top),
        };
        Ok(match id {
            Some(id) if changed => ImportOutcome::Imported(id),
            _ => ImportOutcome::Unchanged,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::{FakeFs, Fs};
    use gpui::TestAppContext;
    use project::Project;
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::Path;
    use text::LineEnding;

    fn init(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    async fn setup(
        cx: &mut TestAppContext,
    ) -> (std::sync::Arc<FakeFs>, Entity<Project>, Entity<Buffer>) {
        // The project must stay alive: it runs the watcher reload.
        init(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/ws", json!({"notes.txt": "alpha\nbeta\ngamma\n"}))
            .await;
        let project = Project::test(fs.clone(), [Path::new("/ws")], cx).await;
        let buffer = project
            .update(cx, |p, cx| p.open_local_buffer("/ws/notes.txt", cx))
            .await
            .unwrap();
        (fs, project, buffer)
    }

    async fn omp_writes(fs: &FakeFs, text: &str) {
        fs.save(Path::new("/ws/notes.txt"), &text.into(), LineEnding::Unix)
            .await
            .unwrap();
    }

    #[gpui::test]
    async fn omp_write_imports_as_one_undoable_transaction(cx: &mut TestAppContext) {
        let (fs, _project, buffer) = setup(cx).await;
        let mark = buffer.update(cx, |b, _| begin(b));
        omp_writes(&fs, "alpha\nBETA\ngamma\n").await;
        let outcome = finish(buffer.clone(), mark, &mut cx.to_async())
            .await
            .unwrap();
        cx.run_until_parked(); // the watcher's own reload must be a no-op now
        let ImportOutcome::Imported(txn) = outcome else {
            panic!("expected one imported transaction, got {outcome:?}");
        };
        buffer.update(cx, |b, cx| {
            assert_eq!(b.text(), "alpha\nBETA\ngamma\n");
            assert!(!b.is_dirty());
            assert_eq!(b.undo(cx), Some(txn), "one undo reverts the agent edit");
            assert_eq!(b.text(), "alpha\nbeta\ngamma\n");
        });
    }

    #[gpui::test]
    async fn watcher_reload_racing_ahead_is_folded_into_the_import(cx: &mut TestAppContext) {
        let (fs, _project, buffer) = setup(cx).await;
        let mark = buffer.update(cx, |b, _| begin(b));
        omp_writes(&fs, "alpha\nBETA\ngamma\n").await;
        cx.run_until_parked(); // watcher reloads first
        assert_eq!(
            buffer.read_with(cx, |b, _| b.text()),
            "alpha\nBETA\ngamma\n"
        );
        let outcome = finish(buffer.clone(), mark, &mut cx.to_async())
            .await
            .unwrap();
        assert!(matches!(outcome, ImportOutcome::Imported(_)), "{outcome:?}");
        buffer.update(cx, |b, cx| {
            b.undo(cx);
            assert_eq!(b.text(), "alpha\nbeta\ngamma\n");
        });
    }

    #[gpui::test]
    async fn unsaved_user_edits_are_never_overwritten(cx: &mut TestAppContext) {
        let (fs, _project, buffer) = setup(cx).await;
        let mark = buffer.update(cx, |b, _| begin(b));
        buffer.update(cx, |b, cx| b.edit([(0..0, "USER ")], None, cx));
        omp_writes(&fs, "alpha\nBETA\ngamma\n").await;
        let outcome = finish(buffer.clone(), mark, &mut cx.to_async())
            .await
            .unwrap();
        assert_eq!(outcome, ImportOutcome::Stale);
        assert_eq!(
            buffer.read_with(cx, |b, _| b.text()),
            "USER alpha\nbeta\ngamma\n"
        );
    }

    #[gpui::test]
    async fn untouched_file_is_unchanged_not_misattributed(cx: &mut TestAppContext) {
        // A prior agent transaction sits on top of the undo stack; an empty
        // reload returns it, which must NOT be credited to the next call.
        let (fs, _project, buffer) = setup(cx).await;
        let mark = buffer.update(cx, |b, _| begin(b));
        omp_writes(&fs, "alpha\nBETA\ngamma\n").await;
        finish(buffer.clone(), mark, &mut cx.to_async())
            .await
            .unwrap();
        let mark = buffer.update(cx, |b, _| begin(b));
        let outcome = finish(buffer.clone(), mark, &mut cx.to_async())
            .await
            .unwrap();
        assert_eq!(outcome, ImportOutcome::Unchanged);
    }
}

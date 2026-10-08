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
use gpui::{AsyncApp, Context, Entity, Subscription};
use language::{Buffer, BufferEvent};
use std::cell::Cell;
use std::rc::Rc;
use text::TransactionId;

/// Buffer state captured when an edit-class tool starts.
#[derive(Clone)]
pub struct Mark {
    start: text::BufferSnapshot,
    start_top: Option<TransactionId>,
    /// The file's disk text when the tool started, when it was read: a
    /// buffer dirty before the call differs from its disk, and only a disk
    /// the tool changed is an outcome.
    disk_at_start: Option<String>,
    /// Set when the buffer was saved during the call: a transaction of the
    /// person's that is clean at [`finish`] and so invisible to the dirty
    /// check. The subscription lives as long as the mark.
    saved: Rc<Cell<bool>>,
    _saved_watch: Rc<Subscription>,
}

impl Mark {
    /// The buffer as it was when the tool started: a file's review baseline
    /// on the task's first edit to it.
    pub fn start(&self) -> &text::BufferSnapshot {
        &self.start
    }

    /// Record the disk text read at the tool's start.
    pub fn set_disk_at_start(&mut self, text: String) {
        self.disk_at_start = Some(text);
    }

    fn disk_at_start(&self) -> String {
        self.disk_at_start
            .clone()
            .unwrap_or_else(|| self.start.text())
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
pub fn begin(buffer: &mut Buffer, cx: &mut Context<Buffer>) -> Mark {
    buffer.finalize_last_transaction();
    let saved = Rc::new(Cell::new(false));
    let watch = cx.subscribe_self({
        let saved = saved.clone();
        move |_, event: &BufferEvent, _| {
            if matches!(event, BufferEvent::Saved) {
                saved.set(true);
            }
        }
    });
    Mark {
        start: buffer.text_snapshot(),
        start_top: top(buffer),
        disk_at_start: None,
        saved,
        _saved_watch: Rc::new(watch),
    }
}

/// [`begin`] for a buffer opened after the tool started, from the disk
/// text the tool started from. If the write already landed in what the
/// buffer loaded, the buffer is set back to that text first, as saved
/// state and outside the undo history, so the write imports as the one
/// transaction on top of it. `None` when the buffer is the person's: they
/// opened it meanwhile and it holds their unsaved text or their undo
/// history, which is never rewritten (ADR-0006); [`begin`] applies and
/// [`finish`] reports `Stale` or `Unchanged`.
pub fn begin_from(
    buffer: &mut Buffer,
    text_at_start: &str,
    cx: &mut Context<Buffer>,
) -> Option<Mark> {
    if buffer.text() != text_at_start {
        if buffer.is_dirty() || top(buffer).is_some() {
            return None;
        }
        buffer.set_text(text_at_start, cx);
        if let Some(txn) = top(buffer) {
            buffer.forget_transaction(txn);
        }
        let mtime = buffer.file().and_then(|f| f.disk_state().mtime());
        buffer.did_reload(buffer.version(), buffer.line_ending(), mtime, cx);
    }
    Some(begin(buffer, cx))
}

/// Load the disk text and import it as one transaction. The import applies
/// the disk diff itself rather than calling `Buffer::reload`: the project's
/// watcher reloads the same change on its own, and a second `reload` cancels
/// the first, so the import would see nothing while the watcher's reload
/// landed after it.
///
/// A file the tool did not write (the disk still holds the text the call
/// started from, read at the mark when the buffer was dirty then) is
/// `Unchanged` whatever the buffer holds: the person's typing in an open
/// file the call never touched is no outcome.
///
/// Edits since [`begin`] are the watcher landing this same write only when
/// they leave the buffer equal to the disk text and the buffer was not saved
/// during the call; then the top transaction is the import. A buffer saved
/// in the window holds a transaction of the person's (typed and autosaved),
/// clean and so invisible to the dirty check, under or inside the watcher's
/// reload or standing alone when the call wrote nothing: nothing is imported
/// and the file is `Stale` (ADR-0006, unsure means STALE). Edits that leave
/// the buffer different from the disk are the person's too.
pub async fn finish(
    buffer: Entity<Buffer>,
    mark: Mark,
    cx: &mut AsyncApp,
) -> Result<ImportOutcome> {
    let load = buffer.read_with(cx, |b, cx| {
        b.file().and_then(|f| f.as_local()).map(|f| f.load(cx))
    });
    let Some(load) = load else {
        return Ok(ImportOutcome::Unchanged);
    };
    let mut disk_text = load.await?;
    text::LineEnding::normalize(&mut disk_text);
    if disk_text == mark.disk_at_start() {
        return Ok(ImportOutcome::Unchanged);
    }
    if buffer.read_with(cx, |b, _| b.is_dirty()) {
        return Ok(ImportOutcome::Stale);
    }
    let diff = buffer
        .read_with(cx, |b, cx| b.diff(disk_text.clone(), cx))
        .await;

    buffer.update(cx, |b, cx| {
        b.finalize_last_transaction();
        let edited_since_mark = b.has_edits_since(mark.start.version());
        let imported = if edited_since_mark {
            if b.text() != disk_text || mark.saved.get() {
                return Ok(ImportOutcome::Stale);
            }
            top(b).filter(|t| Some(*t) != mark.start_top)
        } else {
            let ours = b.apply_diff(diff, cx);
            b.finalize_last_transaction();
            ours
        };
        if b.text() == disk_text {
            let mtime = b.file().and_then(|f| f.disk_state().mtime());
            b.did_reload(b.version(), b.line_ending(), mtime, cx);
        }
        Ok(match imported {
            Some(id) if b.version() != *mark.start.version() => ImportOutcome::Imported(id),
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
        let mark = buffer.update(cx, |b, cx| begin(b, cx));
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
        let mark = buffer.update(cx, |b, cx| begin(b, cx));
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
        let mark = buffer.update(cx, |b, cx| begin(b, cx));
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

    /// A keystroke lands while `finish` is reading the disk: the import must
    /// not take it (the user's text would be marked saved and lost), so the
    /// file is Stale and the buffer stays dirty.
    #[gpui::test]
    async fn a_keystroke_during_the_import_leaves_the_buffer_dirty(cx: &mut TestAppContext) {
        let (fs, _project, buffer) = setup(cx).await;
        let mark = buffer.update(cx, |b, cx| begin(b, cx));
        omp_writes(&fs, "alpha\nBETA\ngamma\n").await;
        let mut acx = cx.to_async();
        let mut finish = std::pin::pin!(finish(buffer.clone(), mark, &mut acx));
        assert!(
            futures::poll!(finish.as_mut()).is_pending(),
            "the dirty check passed; the disk read is in flight"
        );
        buffer.update(cx, |b, cx| b.edit([(0..0, "USER ")], None, cx));
        let outcome = finish.await.unwrap();
        cx.run_until_parked();
        assert_eq!(outcome, ImportOutcome::Stale);
        buffer.read_with(cx, |b, _| {
            assert_eq!(b.text(), "USER alpha\nbeta\ngamma\n");
            assert!(b.is_dirty(), "the user's edit is still unsaved");
        });
    }

    /// Autosave: the user's transaction during the call is already saved,
    /// so the buffer is clean at `finish`. It is still not the agent's.
    #[gpui::test]
    async fn a_saved_user_transaction_during_the_call_is_not_the_agents(cx: &mut TestAppContext) {
        let (fs, project, buffer) = setup(cx).await;
        let mark = buffer.update(cx, |b, cx| begin(b, cx));
        buffer.update(cx, |b, cx| b.edit([(0..0, "USER ")], None, cx));
        project
            .update(cx, |p, cx| p.save_buffer(buffer.clone(), cx))
            .await
            .unwrap();
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

    /// Autosave, then OMP's write keeps the user's text and the watcher
    /// reloads before `finish`: the buffer equals the disk and is clean,
    /// but the saved transaction in the window is the user's, so nothing
    /// is imported and the file is Stale.
    #[gpui::test]
    async fn a_saved_user_edit_the_write_kept_under_the_watcher_is_stale(cx: &mut TestAppContext) {
        let (fs, project, buffer) = setup(cx).await;
        let mark = buffer.update(cx, |b, cx| begin(b, cx));
        buffer.update(cx, |b, cx| b.edit([(5..5, "foo")], None, cx));
        project
            .update(cx, |p, cx| p.save_buffer(buffer.clone(), cx))
            .await
            .unwrap();
        omp_writes(&fs, "ALPHAfoo\nbeta\ngamma\n").await;
        cx.run_until_parked();
        assert_eq!(
            buffer.read_with(cx, |b, _| b.text()),
            "ALPHAfoo\nbeta\ngamma\n",
            "the watcher reloaded"
        );
        let outcome = finish(buffer.clone(), mark, &mut cx.to_async())
            .await
            .unwrap();
        assert_eq!(outcome, ImportOutcome::Stale);
        assert_eq!(
            buffer.read_with(cx, |b, _| b.text()),
            "ALPHAfoo\nbeta\ngamma\n"
        );
    }

    /// The call names the file but leaves its disk alone; the user types
    /// and saves during it. The disk changed since the mark by the user's
    /// save only, which is never the agent's.
    #[gpui::test]
    async fn a_user_save_during_a_call_that_wrote_nothing_is_not_imported(cx: &mut TestAppContext) {
        let (_fs, project, buffer) = setup(cx).await;
        let mark = buffer.update(cx, |b, cx| begin(b, cx));
        buffer.update(cx, |b, cx| b.edit([(0..0, "USER ")], None, cx));
        project
            .update(cx, |p, cx| p.save_buffer(buffer.clone(), cx))
            .await
            .unwrap();
        cx.run_until_parked();
        let outcome = finish(buffer.clone(), mark, &mut cx.to_async())
            .await
            .unwrap();
        assert_eq!(outcome, ImportOutcome::Stale);
        assert_eq!(
            buffer.read_with(cx, |b, _| b.text()),
            "USER alpha\nbeta\ngamma\n"
        );
    }

    /// A file opened after OMP wrote it: the text the tool started from
    /// (read from the disk before the write) is the baseline, so the write
    /// is one undoable transaction and the buffer is clean afterwards.
    #[gpui::test]
    async fn a_buffer_opened_after_the_write_takes_the_pre_write_text_as_baseline(
        cx: &mut TestAppContext,
    ) {
        let (fs, project, _notes) = setup(cx).await;
        fs.insert_tree("/ws", json!({"other.txt": "one\ntwo\n"}))
            .await;
        fs.save(
            Path::new("/ws/other.txt"),
            &"ONE\ntwo\n".into(),
            LineEnding::Unix,
        )
        .await
        .unwrap();
        let buffer = project
            .update(cx, |p, cx| p.open_local_buffer("/ws/other.txt", cx))
            .await
            .unwrap();
        let mark = buffer
            .update(cx, |b, cx| begin_from(b, "one\ntwo\n", cx))
            .expect("a fresh load is set back to the start text");
        assert_eq!(mark.start().text(), "one\ntwo\n");
        let outcome = finish(buffer.clone(), mark, &mut cx.to_async())
            .await
            .unwrap();
        cx.run_until_parked();
        let ImportOutcome::Imported(txn) = outcome else {
            panic!("{outcome:?}");
        };
        buffer.update(cx, |b, cx| {
            assert_eq!(b.text(), "ONE\ntwo\n");
            assert!(!b.is_dirty());
            assert!(!b.has_conflict());
            assert_eq!(b.undo(cx), Some(txn), "one undo is the agent's write");
            assert_eq!(b.text(), "one\ntwo\n");
            assert_eq!(b.undo(cx), None, "nothing under it");
        });
    }

    /// A buffer the person opened and saved their own edits in before the
    /// write landed is theirs: it is not set back to the tool's start text.
    #[gpui::test]
    async fn begin_from_leaves_a_buffer_with_user_history_alone(cx: &mut TestAppContext) {
        let (fs, project, _notes) = setup(cx).await;
        fs.insert_tree("/ws", json!({"other.txt": "one\ntwo\n"}))
            .await;
        let buffer = project
            .update(cx, |p, cx| p.open_local_buffer("/ws/other.txt", cx))
            .await
            .unwrap();
        buffer.update(cx, |b, cx| b.edit([(0..0, "USER ")], None, cx));
        project
            .update(cx, |p, cx| p.save_buffer(buffer.clone(), cx))
            .await
            .unwrap();
        let mark = buffer.update(cx, |b, cx| begin_from(b, "one\ntwo\n", cx));
        assert!(mark.is_none(), "the buffer holds the user's history");
        buffer.read_with(cx, |b, _| {
            assert_eq!(b.text(), "USER one\ntwo\n");
            assert!(b.peek_undo_stack().is_some(), "their undo history is kept");
        });
    }

    /// The import marks the buffer saved at the disk's mtime, so a later
    /// user edit is not a conflict with the file on disk.
    #[gpui::test]
    async fn a_user_edit_after_an_import_is_no_conflict(cx: &mut TestAppContext) {
        let (fs, _project, buffer) = setup(cx).await;
        let mark = buffer.update(cx, |b, cx| begin(b, cx));
        omp_writes(&fs, "alpha\nBETA\ngamma\n").await;
        finish(buffer.clone(), mark, &mut cx.to_async())
            .await
            .unwrap();
        cx.run_until_parked();
        buffer.update(cx, |b, cx| {
            assert!(!b.has_conflict(), "clean after the import");
            b.edit([(0..0, "USER ")], None, cx);
            assert!(b.is_dirty());
            assert!(
                !b.has_conflict(),
                "the user's edit is on top of the imported disk text"
            );
        });
    }

    #[gpui::test]
    async fn untouched_file_is_unchanged_not_misattributed(cx: &mut TestAppContext) {
        // A prior agent transaction sits on top of the undo stack; an empty
        // reload returns it, which must NOT be credited to the next call.
        let (fs, _project, buffer) = setup(cx).await;
        let mark = buffer.update(cx, |b, cx| begin(b, cx));
        omp_writes(&fs, "alpha\nBETA\ngamma\n").await;
        finish(buffer.clone(), mark, &mut cx.to_async())
            .await
            .unwrap();
        let mark = buffer.update(cx, |b, cx| begin(b, cx));
        let outcome = finish(buffer.clone(), mark, &mut cx.to_async())
            .await
            .unwrap();
        assert_eq!(outcome, ImportOutcome::Unchanged);
    }
}

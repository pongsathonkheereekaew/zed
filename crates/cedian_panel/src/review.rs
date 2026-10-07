//! Review Changes over Zed buffers (cedian ARCHITECTURE §15–§18, ADR-0006,
//! S9 U5): one [`TaskReview`] per task, one [`FileReview`] per buffer the
//! task's agent wrote.
//!
//! The baseline of a file is the buffer's `clock::Global` and text just
//! before the task's first agent edit to it (ADR-0006: per task, never git
//! HEAD). Hunks are the buffer's own `edits_since(baseline)`, widened to
//! whole lines, so they are the CRDT's truth, not a second diff engine. A
//! hunk is `Stale` when it intersects a user edit made after the agent last
//! touched the buffer; the user's edits are kept as anchors so a later agent
//! or cedian edit never demotes a stale hunk back to pending.
//!
//! Accept marks. Reject is one `Buffer::edit` putting the baseline lines
//! back, in its own finalized transaction, so native undo restores the
//! agent's text. Reject refuses a stale hunk and a buffer that moved since
//! the hunks were built. Accept all skips stale hunks (decision 7).

use cedian_review::{HunkKey, HunkStatus, StatusRecord};
use collections::HashMap;
use gpui::{App, Entity};
use language::Buffer;
use std::ops::Range;
use std::path::PathBuf;
use text::{Anchor, ToOffset, TransactionId};

/// Review failures (caller-visible).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewError {
    NoFile {
        path: PathBuf,
    },
    BadHunk {
        path: PathBuf,
        index: usize,
    },
    /// Resolving from this status is not allowed (a stale hunk is never
    /// rejected; an interrupted one is never resolved).
    BadTransition {
        status: HunkStatus,
    },
    /// The buffer changed since the hunks were built: rebuild and look again.
    Outdated {
        path: PathBuf,
    },
    NoTurn {
        turn: u32,
    },
}

impl std::fmt::Display for ReviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoFile { path } => write!(f, "{} is not under review", path.display()),
            Self::BadHunk { path, index } => write!(f, "no hunk {index} in {}", path.display()),
            Self::BadTransition {
                status: HunkStatus::Stale,
            } => write!(
                f,
                "hunk is STALE (changed after the agent edit): compare and restore manually"
            ),
            Self::BadTransition { status } => {
                write!(f, "hunk transition not allowed from {status:?}")
            }
            Self::Outdated { path } => write!(
                f,
                "{} changed since the review was built; look again",
                path.display()
            ),
            Self::NoTurn { turn } => write!(f, "no turn {turn}"),
        }
    }
}

impl std::error::Error for ReviewError {}

/// Something the owner records outside the model (a corrections row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewEvent {
    HunkRejected {
        path: PathBuf,
        key: HunkKey,
        turn: u32,
        tool_call_id: String,
    },
    /// Emitted once per hunk identity, however often the review rebuilds.
    UserEditedAgentHunk {
        path: PathBuf,
        key: HunkKey,
        turn: u32,
    },
    TurnReverted {
        turn: u32,
        reverted: usize,
        stale: usize,
    },
}

/// One agent transaction the task made to a buffer.
#[derive(Debug, Clone)]
pub struct AgentTxn {
    pub tool_call_id: String,
    pub turn: u32,
    pub transaction: TransactionId,
}

/// One reviewable change: `old` in baseline offsets, `new` in current ones,
/// both whole lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewHunk {
    pub old: Range<usize>,
    pub new: Range<usize>,
    /// 0-based rows in the current buffer.
    pub rows: Range<u32>,
    pub old_text: String,
    pub new_text: String,
    pub status: HunkStatus,
    /// The agent calls whose edits this hunk covers, oldest first.
    pub tool_call_ids: Vec<String>,
}

impl ReviewHunk {
    fn key(&self, baseline: &text::BufferSnapshot) -> HunkKey {
        let start = baseline.offset_to_point(self.old.start).row as usize;
        let end = baseline.offset_to_point(self.old.end).row as usize;
        HunkKey {
            before_start: start,
            before_count: end.saturating_sub(start),
            after_text: self.new_text.trim_end_matches('\n').to_string(),
        }
    }
}

/// What a turn did, so it can be reverted (§18 "Revert turn").
#[derive(Debug, Clone)]
pub enum TurnKind {
    Prompt,
    /// The revert of turn `of`: one transaction per buffer it touched.
    Revert {
        of: u32,
        transactions: Vec<(Entity<Buffer>, TransactionId)>,
    },
}

/// Per-buffer review state.
pub struct FileReview {
    buffer: Entity<Buffer>,
    baseline: text::BufferSnapshot,
    agent_txns: Vec<AgentTxn>,
    /// Every edit up to here is accounted for: the agent's, cedian's, or
    /// folded into `user_edits`.
    last_seen: clock::Global,
    user_edits: Vec<Range<Anchor>>,
    /// Set when OMP wrote the disk while the buffer had unsaved edits, so
    /// nothing was imported (decision 5).
    stale_import: Option<String>,
    resolved: HashMap<HunkKey, HunkStatus>,
    reported_stale: collections::HashSet<HunkKey>,
    hunks: Vec<ReviewHunk>,
    built_at: clock::Global,
}

impl FileReview {
    pub fn hunks(&self) -> &[ReviewHunk] {
        &self.hunks
    }

    pub fn buffer(&self) -> &Entity<Buffer> {
        &self.buffer
    }

    /// Why the whole file is STALE, when an import was refused.
    pub fn stale_import(&self) -> Option<&str> {
        self.stale_import.as_deref()
    }

    pub fn agent_txns(&self) -> &[AgentTxn] {
        &self.agent_txns
    }

    fn fold_user_edits(&mut self, buffer: &Buffer) {
        let snapshot = buffer.text_snapshot();
        for (_, range) in snapshot.anchored_edits_since::<usize>(&self.last_seen) {
            self.user_edits.push(range);
        }
        self.last_seen = snapshot.version().clone();
    }

    fn rebuild(&mut self, buffer: &Buffer) -> Vec<HunkKey> {
        let snapshot = buffer.text_snapshot();
        let edits: Vec<text::Edit<usize>> = snapshot
            .edits_since::<usize>(self.baseline.version())
            .collect();
        let user: Vec<Range<usize>> = self
            .user_edits
            .iter()
            .map(|r| r.start.to_offset(&snapshot)..r.end.to_offset(&snapshot))
            .collect();
        let agent: Vec<(String, Vec<Range<usize>>)> = self
            .agent_txns
            .iter()
            .map(|t| {
                (
                    t.tool_call_id.clone(),
                    buffer
                        .edited_ranges_for_transaction_id::<usize>(t.transaction)
                        .collect(),
                )
            })
            .collect();
        let mut newly_stale = Vec::new();
        // The CRDT reports a rejected region as an edit too (its lines were
        // deleted and put back), so equal text is no hunk.
        self.hunks = line_hunks(&self.baseline, &snapshot, &edits)
            .into_iter()
            .filter_map(|(old, new)| {
                let old_text: String = self.baseline.text_for_range(old.clone()).collect();
                let new_text: String = snapshot.text_for_range(new.clone()).collect();
                (old_text != new_text).then_some((old, new, old_text, new_text))
            })
            .map(|(old, new, old_text, new_text)| {
                let rows = snapshot.offset_to_point(new.start).row
                    ..snapshot
                        .offset_to_point(new.end)
                        .row
                        .max(snapshot.offset_to_point(new.start).row + 1);
                let mut hunk = ReviewHunk {
                    old,
                    new,
                    rows,
                    old_text,
                    new_text,
                    status: HunkStatus::Pending,
                    tool_call_ids: Vec::new(),
                };
                hunk.tool_call_ids = agent
                    .iter()
                    .filter(|(_, ranges)| ranges.iter().any(|r| touches(&hunk.new, r)))
                    .map(|(id, _)| id.clone())
                    .collect();
                let key = hunk.key(&self.baseline);
                hunk.status = match self.resolved.get(&key) {
                    Some(status) => *status,
                    None if user.iter().any(|r| touches(&hunk.new, r)) => HunkStatus::Stale,
                    None if hunk.tool_call_ids.is_empty() => HunkStatus::Unattributed,
                    None => HunkStatus::Pending,
                };
                if hunk.status == HunkStatus::Stale && self.reported_stale.insert(key.clone()) {
                    newly_stale.push(key);
                }
                hunk
            })
            .collect();
        self.built_at = snapshot.version().clone();
        newly_stale
    }
}

/// Two ranges touch when they overlap, or one is empty and sits inside or on
/// an edge of the other (an insertion next to a hunk makes it stale: unsure
/// means STALE, never an overwrite).
fn touches(a: &Range<usize>, b: &Range<usize>) -> bool {
    if a.is_empty() || b.is_empty() {
        a.start <= b.end && b.start <= a.end
    } else {
        a.start < b.end && b.start < a.end
    }
}

/// Widen edits to whole lines and merge the ones that then touch. The text
/// outside every edit is the same in both snapshots, so widening by the same
/// distance on each side keeps `old` and `new` aligned.
fn line_hunks(
    old: &text::BufferSnapshot,
    new: &text::BufferSnapshot,
    edits: &[text::Edit<usize>],
) -> Vec<(Range<usize>, Range<usize>)> {
    let at_line_start =
        |s: &text::BufferSnapshot, o: usize| o == 0 || s.chars_at(o - 1).next() == Some('\n');
    let mut out: Vec<(Range<usize>, Range<usize>)> = Vec::new();
    for edit in edits {
        let back = if at_line_start(new, edit.new.start) {
            0
        } else {
            edit.new.start
                - new.point_to_offset(text::Point::new(new.offset_to_point(edit.new.start).row, 0))
        };
        let forward = if at_line_start(new, edit.new.end) && at_line_start(old, edit.old.end) {
            0
        } else {
            let row = new.offset_to_point(edit.new.end).row;
            let line_end = if row < new.max_point().row {
                new.point_to_offset(text::Point::new(row + 1, 0))
            } else {
                new.len()
            };
            line_end - edit.new.end
        };
        let new_range = edit.new.start - back..edit.new.end + forward;
        let old_range = edit.old.start - back..(edit.old.end + forward).min(old.len());
        match out.last_mut() {
            Some((o, n)) if new_range.start <= n.end => {
                n.end = n.end.max(new_range.end);
                o.end = o.end.max(old_range.end);
            }
            _ => out.push((old_range, new_range)),
        }
    }
    out
}

/// One task's review: the files its agent wrote and their hunks.
pub struct TaskReview {
    task_id: String,
    files: Vec<FileReview>,
    turns: Vec<(u32, TurnKind)>,
    events: Vec<ReviewEvent>,
}

impl TaskReview {
    pub fn new(task_id: &str) -> Self {
        Self {
            task_id: task_id.to_string(),
            files: Vec::new(),
            turns: Vec::new(),
            events: Vec::new(),
        }
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn files(&self) -> &[FileReview] {
        &self.files
    }

    pub fn is_empty(&self) -> bool {
        self.files
            .iter()
            .all(|f| f.hunks.is_empty() && f.stale_import.is_none())
    }

    /// Events since the last drain, oldest first.
    pub fn drain_events(&mut self) -> Vec<ReviewEvent> {
        std::mem::take(&mut self.events)
    }

    /// A prompt turn begins; returns its number.
    pub fn begin_turn(&mut self) -> u32 {
        let n = self.turns.last().map_or(1, |(n, _)| n + 1);
        self.turns.push((n, TurnKind::Prompt));
        n
    }

    pub fn current_turn(&self) -> u32 {
        self.turns.last().map_or(0, |(n, _)| *n)
    }

    fn file_index(&self, buffer: &Entity<Buffer>) -> Option<usize> {
        self.files.iter().position(|f| &f.buffer == buffer)
    }

    /// Account for the user's edits to a tracked buffer up to now. Called
    /// before any agent write can land (the tool's start).
    pub fn observe(&mut self, buffer: &Entity<Buffer>, cx: &App) {
        if let Some(i) = self.file_index(buffer) {
            self.files[i].fold_user_edits(buffer.read(cx));
        }
    }

    /// An imported agent transaction. `baseline` is the buffer as it was when
    /// the tool started; it becomes the file's baseline on the first edit.
    pub fn agent_edited(
        &mut self,
        buffer: &Entity<Buffer>,
        tool_call_id: &str,
        transaction: TransactionId,
        baseline: text::BufferSnapshot,
        cx: &App,
    ) {
        let turn = self.current_turn();
        let i = match self.file_index(buffer) {
            Some(i) => i,
            None => {
                self.files.push(FileReview {
                    buffer: buffer.clone(),
                    last_seen: baseline.version().clone(),
                    baseline,
                    agent_txns: Vec::new(),
                    user_edits: Vec::new(),
                    stale_import: None,
                    resolved: HashMap::default(),
                    reported_stale: collections::HashSet::default(),
                    hunks: Vec::new(),
                    built_at: clock::Global::new(),
                });
                self.files.len() - 1
            }
        };
        let file = &mut self.files[i];
        file.agent_txns.push(AgentTxn {
            tool_call_id: tool_call_id.to_string(),
            turn,
            transaction,
        });
        file.last_seen = buffer.read(cx).version();
        self.rebuild(cx);
    }

    /// OMP wrote the disk while the buffer had unsaved edits: nothing was
    /// imported and the file is STALE as a whole.
    pub fn import_refused(&mut self, buffer: &Entity<Buffer>, tool_call_id: &str, cx: &App) {
        let reason = format!(
            "OMP wrote the file on disk (call {tool_call_id}) while the buffer had unsaved edits; \
             nothing was imported"
        );
        match self.file_index(buffer) {
            Some(i) => self.files[i].stale_import = Some(reason),
            None => {
                let snapshot = buffer.read(cx).text_snapshot();
                self.files.push(FileReview {
                    buffer: buffer.clone(),
                    last_seen: snapshot.version().clone(),
                    baseline: snapshot,
                    agent_txns: Vec::new(),
                    user_edits: Vec::new(),
                    stale_import: Some(reason),
                    resolved: HashMap::default(),
                    reported_stale: collections::HashSet::default(),
                    hunks: Vec::new(),
                    built_at: clock::Global::new(),
                });
            }
        }
    }

    /// Recompute every file's hunks from its buffer.
    pub fn rebuild(&mut self, cx: &App) {
        for file in &mut self.files {
            let buffer = file.buffer.read(cx);
            file.fold_user_edits(buffer);
            let path = path_of(buffer, cx);
            let turn = file.agent_txns.last().map_or(0, |t| t.turn);
            for key in file.rebuild(buffer) {
                self.events.push(ReviewEvent::UserEditedAgentHunk {
                    path: path.clone(),
                    key,
                    turn,
                });
            }
        }
    }

    pub fn path(&self, file: &FileReview, cx: &App) -> PathBuf {
        path_of(file.buffer.read(cx), cx)
    }

    fn file_by_path(&self, path: &std::path::Path, cx: &App) -> Result<usize, ReviewError> {
        self.files
            .iter()
            .position(|f| path_of(f.buffer.read(cx), cx) == path)
            .ok_or_else(|| ReviewError::NoFile {
                path: path.to_path_buf(),
            })
    }

    /// Accept one hunk: mark only, the code is already in the buffer.
    pub fn accept(
        &mut self,
        path: &std::path::Path,
        index: usize,
        cx: &App,
    ) -> Result<(), ReviewError> {
        let i = self.file_by_path(path, cx)?;
        let file = &mut self.files[i];
        let hunk = file.hunks.get(index).ok_or_else(|| ReviewError::BadHunk {
            path: path.to_path_buf(),
            index,
        })?;
        match hunk.status {
            HunkStatus::Pending | HunkStatus::Unattributed | HunkStatus::Stale => {
                let key = hunk.key(&file.baseline);
                file.hunks[index].status = HunkStatus::Accepted;
                file.resolved.insert(key, HunkStatus::Accepted);
                Ok(())
            }
            HunkStatus::Interrupted => Err(ReviewError::BadTransition {
                status: hunk.status,
            }),
            HunkStatus::Accepted | HunkStatus::Rejected => Ok(()),
        }
    }

    /// Reject one hunk: put the baseline lines back as one finalized
    /// transaction. Refuses `Stale` and `Interrupted`, and a buffer that
    /// moved since the hunks were built.
    pub fn reject(
        &mut self,
        path: &std::path::Path,
        index: usize,
        cx: &mut App,
    ) -> Result<TransactionId, ReviewError> {
        let i = self.file_by_path(path, cx)?;
        let file = &mut self.files[i];
        let hunk = file
            .hunks
            .get(index)
            .cloned()
            .ok_or_else(|| ReviewError::BadHunk {
                path: path.to_path_buf(),
                index,
            })?;
        if matches!(
            hunk.status,
            HunkStatus::Stale
                | HunkStatus::Interrupted
                | HunkStatus::Accepted
                | HunkStatus::Rejected
        ) {
            return Err(ReviewError::BadTransition {
                status: hunk.status,
            });
        }
        if file.buffer.read(cx).version() != file.built_at {
            return Err(ReviewError::Outdated {
                path: path.to_path_buf(),
            });
        }
        let key = hunk.key(&file.baseline);
        let txn = restore(
            &file.buffer,
            vec![(hunk.new.clone(), hunk.old_text.clone())],
            cx,
        );
        file.last_seen = file.buffer.read(cx).version();
        file.resolved.insert(key.clone(), HunkStatus::Rejected);
        let turn = file.agent_txns.last().map_or(0, |t| t.turn);
        self.events.push(ReviewEvent::HunkRejected {
            path: path.to_path_buf(),
            key,
            turn,
            tool_call_id: hunk.tool_call_ids.last().cloned().unwrap_or_default(),
        });
        self.rebuild(cx);
        Ok(txn)
    }

    /// Accept every `Pending` hunk. Skips `Stale` (each needs its own
    /// accept), `Unattributed` and `Interrupted` (§17 R2).
    pub fn accept_all(&mut self, cx: &App) -> Vec<(PathBuf, usize)> {
        let mut accepted = Vec::new();
        for file in &mut self.files {
            let path = path_of(file.buffer.read(cx), cx);
            for (i, hunk) in file.hunks.iter_mut().enumerate() {
                if hunk.status == HunkStatus::Pending {
                    let key = hunk.key(&file.baseline);
                    hunk.status = HunkStatus::Accepted;
                    file.resolved.insert(key, HunkStatus::Accepted);
                    accepted.push((path.clone(), i));
                }
            }
        }
        accepted
    }

    /// Revert turn `n`: reject every pending hunk the turn's calls produced,
    /// one transaction per buffer, so one undo per buffer redoes it. Stale
    /// hunks are skipped and counted. Reverting a revert undoes its
    /// transactions, which puts the turn's text back.
    pub fn revert_turn(&mut self, n: u32, cx: &mut App) -> Result<ReviewEvent, ReviewError> {
        let kind = self
            .turns
            .iter()
            .find(|(t, _)| *t == n)
            .map(|(_, k)| k.clone())
            .ok_or(ReviewError::NoTurn { turn: n })?;
        let (reverted, stale, transactions) = match kind {
            TurnKind::Revert { transactions, .. } => {
                let mut undone = 0;
                for (buffer, txn) in &transactions {
                    if buffer.update(cx, |b, cx| b.undo_transaction(*txn, cx)) {
                        undone += 1;
                    }
                    if let Some(i) = self.file_index(buffer) {
                        self.files[i].last_seen = buffer.read(cx).version();
                    }
                }
                self.rebuild(cx);
                (undone, 0, Vec::new())
            }
            TurnKind::Prompt => {
                self.rebuild(cx);
                let mut reverted = 0;
                let mut stale = 0;
                let mut transactions = Vec::new();
                for file in &mut self.files {
                    let calls: Vec<&str> = file
                        .agent_txns
                        .iter()
                        .filter(|t| t.turn == n)
                        .map(|t| t.tool_call_id.as_str())
                        .collect();
                    let mut edits = Vec::new();
                    let mut keys = Vec::new();
                    for hunk in &file.hunks {
                        if !hunk
                            .tool_call_ids
                            .iter()
                            .any(|id| calls.contains(&id.as_str()))
                        {
                            continue;
                        }
                        match hunk.status {
                            HunkStatus::Pending | HunkStatus::Unattributed => {
                                edits.push((hunk.new.clone(), hunk.old_text.clone()));
                                keys.push(hunk.key(&file.baseline));
                            }
                            HunkStatus::Stale => stale += 1,
                            _ => {}
                        }
                    }
                    if edits.is_empty() {
                        continue;
                    }
                    reverted += edits.len();
                    let txn = restore(&file.buffer, edits, cx);
                    file.last_seen = file.buffer.read(cx).version();
                    for key in keys {
                        file.resolved.insert(key, HunkStatus::Rejected);
                    }
                    transactions.push((file.buffer.clone(), txn));
                }
                self.rebuild(cx);
                (reverted, stale, transactions)
            }
        };
        let next = self.turns.last().map_or(1, |(n, _)| n + 1);
        self.turns.push((
            next,
            TurnKind::Revert {
                of: n,
                transactions,
            },
        ));
        let event = ReviewEvent::TurnReverted {
            turn: n,
            reverted,
            stale,
        };
        self.events.push(event.clone());
        Ok(event)
    }

    /// Resolutions to persist.
    pub fn status_records(&self, cx: &App) -> Vec<StatusRecord> {
        let mut out: Vec<StatusRecord> = self
            .files
            .iter()
            .flat_map(|f| {
                let path = path_of(f.buffer.read(cx), cx);
                f.resolved.iter().map(move |(key, status)| StatusRecord {
                    path: path.clone(),
                    key: key.clone(),
                    status: *status,
                })
            })
            .collect();
        out.sort_by(|a, b| {
            (&a.path, a.key.before_start, &a.key.after_text).cmp(&(
                &b.path,
                b.key.before_start,
                &b.key.after_text,
            ))
        });
        out
    }

    /// Restore persisted resolutions (applied on the next rebuild).
    pub fn restore_statuses(&mut self, records: Vec<StatusRecord>, cx: &App) {
        for record in records {
            if let Ok(i) = self.file_by_path(&record.path, cx) {
                self.files[i].resolved.insert(record.key, record.status);
            }
        }
        self.rebuild(cx);
    }
}

/// Replace `edits` (current offsets → replacement) as one finalized
/// transaction and return its id.
fn restore(
    buffer: &Entity<Buffer>,
    edits: Vec<(Range<usize>, String)>,
    cx: &mut App,
) -> TransactionId {
    buffer.update(cx, |b, cx| {
        b.finalize_last_transaction();
        b.start_transaction();
        b.edit(edits, None, cx);
        let txn = b
            .end_transaction(cx)
            .expect("the edit opened a transaction");
        b.finalize_last_transaction();
        txn
    })
}

fn path_of(buffer: &Buffer, cx: &App) -> PathBuf {
    buffer.file().map(|f| f.full_path(cx)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::{self, ImportOutcome};
    use fs::{FakeFs, Fs};
    use gpui::TestAppContext;
    use project::Project;
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::Path;
    use std::sync::Arc;
    use text::LineEnding;

    const ORIGINAL: &str = "alpha\nbeta\ngamma\n";
    const NOTES: &str = "/ws/notes.txt";

    struct Fixture {
        fs: Arc<FakeFs>,
        project: Entity<Project>,
        buffer: Entity<Buffer>,
        review: TaskReview,
    }

    async fn setup(cx: &mut TestAppContext) -> Fixture {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/ws", json!({"notes.txt": ORIGINAL})).await;
        let project = Project::test(fs.clone(), [Path::new("/ws")], cx).await;
        let buffer = project
            .update(cx, |p, cx| p.open_local_buffer(NOTES, cx))
            .await
            .unwrap();
        let mut review = TaskReview::new("task-1");
        review.begin_turn();
        Fixture {
            fs,
            project,
            buffer,
            review,
        }
    }

    /// OMP's edit tool writes the disk; the panel's ToolStart/ToolEnd path
    /// imports it as one transaction attributed to `call`.
    async fn agent_writes(f: &mut Fixture, call: &str, text: &str, cx: &mut TestAppContext) {
        cx.update(|cx| f.review.observe(&f.buffer, cx));
        let mark = f.buffer.update(cx, |b, _| import::begin(b));
        let baseline = mark.start().clone();
        f.fs.save(Path::new(NOTES), &text.into(), LineEnding::Unix)
            .await
            .unwrap();
        let outcome = import::finish(f.buffer.clone(), mark, &mut cx.to_async())
            .await
            .unwrap();
        cx.run_until_parked();
        match outcome {
            ImportOutcome::Imported(txn) => {
                cx.update(|cx| f.review.agent_edited(&f.buffer, call, txn, baseline, cx))
            }
            ImportOutcome::Stale => cx.update(|cx| f.review.import_refused(&f.buffer, call, cx)),
            ImportOutcome::Unchanged => panic!("the write changed nothing"),
        }
    }

    fn user_types(f: &Fixture, range: Range<usize>, text: &str, cx: &mut TestAppContext) {
        f.buffer.update(cx, |b, cx| {
            b.edit([(range, text)], None, cx);
        });
    }

    fn text(f: &Fixture, cx: &mut TestAppContext) -> String {
        f.buffer.read_with(cx, |b, _| b.text())
    }

    fn statuses(f: &Fixture) -> Vec<HunkStatus> {
        f.review.files()[0]
            .hunks()
            .iter()
            .map(|h| h.status)
            .collect()
    }

    #[gpui::test]
    async fn agent_edit_shows_one_attributed_hunk(cx: &mut TestAppContext) {
        let mut f = setup(cx).await;
        agent_writes(&mut f, "c1", "alpha\nBETA\ngamma\n", cx).await;
        assert_eq!(f.review.files().len(), 1);
        let hunks = f.review.files()[0].hunks();
        assert_eq!(hunks.len(), 1, "{hunks:?}");
        assert_eq!(hunks[0].status, HunkStatus::Pending);
        assert_eq!(hunks[0].rows, 1..2);
        assert_eq!(
            (hunks[0].old_text.as_str(), hunks[0].new_text.as_str()),
            ("beta\n", "BETA\n")
        );
        assert_eq!(hunks[0].tool_call_ids, vec!["c1".to_string()]);
    }

    #[gpui::test]
    async fn reject_restores_baseline_and_logs_correction(cx: &mut TestAppContext) {
        let mut f = setup(cx).await;
        agent_writes(&mut f, "c1", "alpha\nBETA\ngamma\n", cx).await;
        let path = cx.update(|cx| f.review.path(&f.review.files()[0], cx));
        cx.update(|cx| f.review.reject(&path, 0, cx)).unwrap();
        assert_eq!(text(&f, cx), ORIGINAL, "reject restored the baseline");
        let events = f.review.drain_events();
        assert_eq!(
            events,
            vec![ReviewEvent::HunkRejected {
                path,
                key: HunkKey {
                    before_start: 1,
                    before_count: 1,
                    after_text: "BETA".to_string()
                },
                turn: 1,
                tool_call_id: "c1".to_string(),
            }]
        );
        assert!(
            f.review.files()[0].hunks().is_empty(),
            "no open hunk after reject"
        );
    }

    #[gpui::test]
    async fn reject_is_one_native_undo(cx: &mut TestAppContext) {
        let mut f = setup(cx).await;
        agent_writes(&mut f, "c1", "alpha\nBETA\ngamma\n", cx).await;
        let path = cx.update(|cx| f.review.path(&f.review.files()[0], cx));
        let txn = cx.update(|cx| f.review.reject(&path, 0, cx)).unwrap();
        f.buffer.update(cx, |b, cx| {
            assert_eq!(b.undo(cx), Some(txn), "one undo is the reject");
            assert_eq!(
                b.text(),
                "alpha\nBETA\ngamma\n",
                "undo brings the agent text back"
            );
        });
        cx.update(|cx| f.review.rebuild(cx));
        assert_eq!(
            statuses(&f),
            vec![HunkStatus::Rejected],
            "the resolution is remembered"
        );
    }

    #[gpui::test]
    async fn user_edit_over_agent_hunk_is_stale(cx: &mut TestAppContext) {
        let mut f = setup(cx).await;
        agent_writes(&mut f, "c1", "alpha\nBETA\ngamma\n", cx).await;
        user_types(&f, 10..10, " by user", cx);
        cx.update(|cx| f.review.rebuild(cx));
        assert_eq!(statuses(&f), vec![HunkStatus::Stale]);
    }

    #[gpui::test]
    async fn stale_event_once(cx: &mut TestAppContext) {
        let mut f = setup(cx).await;
        agent_writes(&mut f, "c1", "alpha\nBETA\ngamma\n", cx).await;
        user_types(&f, 10..10, " by user", cx);
        cx.update(|cx| {
            f.review.rebuild(cx);
            f.review.rebuild(cx);
        });
        let path = cx.update(|cx| f.review.path(&f.review.files()[0], cx));
        let stale: Vec<_> = f
            .review
            .drain_events()
            .into_iter()
            .filter(|e| matches!(e, ReviewEvent::UserEditedAgentHunk { .. }))
            .collect();
        assert_eq!(
            stale,
            vec![ReviewEvent::UserEditedAgentHunk {
                path,
                key: HunkKey {
                    before_start: 1,
                    before_count: 1,
                    after_text: "BETA by user".to_string()
                },
                turn: 1,
            }],
            "one event per edited hunk, however often the review rebuilds"
        );
    }

    #[gpui::test]
    async fn reject_of_stale_hunk_refused(cx: &mut TestAppContext) {
        let mut f = setup(cx).await;
        agent_writes(&mut f, "c1", "alpha\nBETA\ngamma\n", cx).await;
        user_types(&f, 10..10, " by user", cx);
        cx.update(|cx| f.review.rebuild(cx));
        let path = cx.update(|cx| f.review.path(&f.review.files()[0], cx));
        assert_eq!(
            cx.update(|cx| f.review.reject(&path, 0, cx)),
            Err(ReviewError::BadTransition {
                status: HunkStatus::Stale
            })
        );
        assert_eq!(
            text(&f, cx),
            "alpha\nBETA by user\ngamma\n",
            "the user's line survives"
        );
    }

    #[gpui::test]
    async fn accept_stale_keeps_buffer_and_persists(cx: &mut TestAppContext) {
        let mut f = setup(cx).await;
        agent_writes(&mut f, "c1", "alpha\nBETA\ngamma\n", cx).await;
        user_types(&f, 10..10, " by user", cx);
        cx.update(|cx| f.review.rebuild(cx));
        let path = cx.update(|cx| f.review.path(&f.review.files()[0], cx));
        cx.update(|cx| f.review.accept(&path, 0, cx)).unwrap();
        cx.update(|cx| f.review.rebuild(cx));
        assert_eq!(statuses(&f), vec![HunkStatus::Accepted]);
        assert_eq!(
            text(&f, cx),
            "alpha\nBETA by user\ngamma\n",
            "accept keeps the buffer"
        );
        // A fresh review of the same buffer (a reopened task) gets the
        // resolution back from the persisted records.
        let records = cx.update(|cx| f.review.status_records(cx));
        let mut fresh = TaskReview::new("task-1");
        fresh.begin_turn();
        let txn = f.review.files()[0].agent_txns()[0].transaction;
        let baseline = f.review.files()[0].baseline.clone();
        cx.update(|cx| {
            fresh.agent_edited(&f.buffer, "c1", txn, baseline, cx);
            fresh.restore_statuses(records, cx);
        });
        assert_eq!(
            fresh.files()[0].hunks()[0].status,
            HunkStatus::Accepted,
            "accepted after reopen"
        );
    }

    #[gpui::test]
    async fn baseline_excludes_pre_task_user_edits(cx: &mut TestAppContext) {
        let mut f = setup(cx).await;
        // The user's own change, saved before the turn (Agent Sync, §13).
        user_types(&f, 0..5, "ALPHA", cx);
        f.project
            .update(cx, |p, cx| p.save_buffer(f.buffer.clone(), cx))
            .await
            .unwrap();
        agent_writes(&mut f, "c1", "ALPHA\nBETA\ngamma\n", cx).await;
        let hunks = f.review.files()[0].hunks();
        assert_eq!(hunks.len(), 1, "{hunks:?}");
        assert_eq!(hunks[0].rows, 1..2, "the user's line 1 is not a hunk");
        assert_eq!(hunks[0].status, HunkStatus::Pending);
    }

    #[gpui::test]
    async fn reject_refuses_a_buffer_that_moved(cx: &mut TestAppContext) {
        let mut f = setup(cx).await;
        agent_writes(&mut f, "c1", "alpha\nBETA\ngamma\n", cx).await;
        let path = cx.update(|cx| f.review.path(&f.review.files()[0], cx));
        user_types(&f, 0..0, "zero\n", cx);
        assert!(matches!(
            cx.update(|cx| f.review.reject(&path, 0, cx)),
            Err(ReviewError::Outdated { .. })
        ));
        assert_eq!(text(&f, cx), "zero\nalpha\nBETA\ngamma\n");
    }

    #[gpui::test]
    async fn accept_all_skips_stale(cx: &mut TestAppContext) {
        let mut f = setup(cx).await;
        agent_writes(&mut f, "c1", "ALPHA\nbeta\nGAMMA\n", cx).await;
        user_types(&f, 11..11, "!", cx);
        cx.update(|cx| f.review.rebuild(cx));
        assert_eq!(statuses(&f), vec![HunkStatus::Pending, HunkStatus::Stale]);
        let accepted = cx.update(|cx| f.review.accept_all(cx));
        assert_eq!(accepted.len(), 1);
        assert_eq!(statuses(&f), vec![HunkStatus::Accepted, HunkStatus::Stale]);
    }

    #[gpui::test]
    async fn dirty_buffer_import_is_stale_for_the_file(cx: &mut TestAppContext) {
        let mut f = setup(cx).await;
        user_types(&f, 0..0, "USER ", cx);
        agent_writes(&mut f, "c1", "alpha\nBETA\ngamma\n", cx).await;
        assert_eq!(
            text(&f, cx),
            "USER alpha\nbeta\ngamma\n",
            "nothing imported"
        );
        let file = &f.review.files()[0];
        assert!(
            file.stale_import().unwrap().contains("c1"),
            "{:?}",
            file.stale_import()
        );
        assert!(file.hunks().is_empty());
        assert!(!f.review.is_empty(), "the file still shows in review");
    }
}

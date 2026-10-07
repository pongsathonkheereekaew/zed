//! Revert one turn's change to one file (P6, ADR-0026 decision 3).
//!
//! A line-level 3-way merge: the turn turned `before` into `after`; the file
//! is now `current`. Each hunk of `diff(before, after)` is put back only
//! where `diff(after, current)` does not touch it — anything changed since
//! (by the user or a later turn) is `STALE` and left exactly as it is.
//! Revert never overwrites work it did not make.

use crate::diff::{Hunk, line_diff};

/// Outcome for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevertFile {
    /// New file text (== `current` when nothing could be reverted).
    pub text: String,
    /// Turn hunks put back.
    pub reverted: usize,
    /// Turn hunks skipped because something changed them since; ranges are
    /// in `after` (post-turn) line coordinates.
    pub stale: Vec<Hunk>,
}

/// Revert the `before → after` change inside `current`.
pub fn revert_file(before: &str, after: &str, current: &str) -> RevertFile {
    let turn = line_diff(before, after);
    let since = line_diff(after, current);
    let before_lines = segments(before);
    let mut lines = segments(current);
    let mut stale = Vec::new();
    // (start in current, line count in current, replacement) — applied bottom-up.
    let mut patches: Vec<(usize, usize, Vec<String>)> = Vec::new();
    for hunk in &turn {
        let (a_start, a_end) = (hunk.after_start, hunk.after_start + hunk.after_count);
        if since.iter().any(|s| touches(a_start, a_end, s)) {
            stale.push(hunk.clone());
            continue;
        }
        // Shift by every later change that sits wholly before this hunk.
        let shift: isize = since
            .iter()
            .filter(|s| s.before_start + s.before_count <= a_start)
            .map(|s| s.after_count as isize - s.before_count as isize)
            .sum();
        let start = (a_start as isize + shift) as usize;
        let replacement = before_lines
            .iter()
            .skip(hunk.before_start)
            .take(hunk.before_count)
            .cloned()
            .collect();
        patches.push((start, hunk.after_count, replacement));
    }
    let reverted = patches.len();
    for (start, count, replacement) in patches.into_iter().rev() {
        let end = (start + count).min(lines.len());
        lines.splice(start.min(lines.len())..end, replacement);
    }
    RevertFile {
        text: join(lines),
        reverted,
        stale,
    }
}

/// Does a later change `s` (in `after` coordinates) touch `[a_start, a_end)`?
/// Pure insertions count when they land inside or on an edge of the range —
/// conservative on purpose: unsure means `STALE`, never an overwrite.
fn touches(a_start: usize, a_end: usize, s: &Hunk) -> bool {
    let (s_start, s_end) = (s.before_start, s.before_start + s.before_count);
    if a_start < a_end && s_start < s_end {
        a_start.max(s_start) < a_end.min(s_end)
    } else {
        s_start <= a_end && a_start <= s_end
    }
}

/// Lines with their own line endings, so the text round-trips exactly.
fn segments(text: &str) -> Vec<String> {
    text.split_inclusive('\n').map(str::to_string).collect()
}

/// Rejoin, giving any unterminated line that is no longer last a newline.
fn join(lines: Vec<String>) -> String {
    let last = lines.len().saturating_sub(1);
    let mut out = String::new();
    for (i, line) in lines.into_iter().enumerate() {
        out.push_str(&line);
        if i < last && !line.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: &str = "one\ntwo\nthree\nfour\nfive\n";

    #[test]
    fn untouched_turn_reverts_cleanly() {
        let after = "ONE\ntwo\nthree\nfour\nFIVE\n";
        let r = revert_file(BEFORE, after, after);
        assert_eq!(r.text, BEFORE);
        assert_eq!((r.reverted, r.stale.len()), (2, 0));
    }

    #[test]
    fn later_edit_elsewhere_shifts_but_survives() {
        let after = "one\ntwo\nthree\nfour\nFIVE\n";
        // Since the turn: two lines inserted at the top, `three` changed.
        let current = "zero\nzero2\none\ntwo\nTHREE!\nfour\nFIVE\n";
        let r = revert_file(BEFORE, after, current);
        assert_eq!(r.text, "zero\nzero2\none\ntwo\nTHREE!\nfour\nfive\n");
        assert_eq!((r.reverted, r.stale.len()), (1, 0));
    }

    #[test]
    fn edit_over_the_turn_hunk_is_stale_and_kept() {
        let after = "ONE\ntwo\nthree\nfour\nFIVE\n";
        let current = "ONE\ntwo\nthree\nfour\nFIVE (mine)\n";
        let r = revert_file(BEFORE, after, current);
        assert_eq!(r.text, "one\ntwo\nthree\nfour\nFIVE (mine)\n");
        assert_eq!(r.reverted, 1);
        assert_eq!(r.stale.len(), 1);
        assert_eq!(r.stale[0].after_start, 4);
    }

    #[test]
    fn insertion_next_to_the_hunk_is_stale() {
        let after = "one\nTWO\nthree\nfour\nfive\n";
        let current = "one\nTWO\nnew\nthree\nfour\nfive\n";
        let r = revert_file(BEFORE, after, current);
        assert_eq!(r.text, current, "unsure → keep");
        assert_eq!(r.stale.len(), 1);
    }

    #[test]
    fn created_file_reverts_to_empty_and_inserted_lines_go() {
        assert_eq!(revert_file("", "new\n", "new\n").text, "");
        let after = "one\ntwo\nadded\nthree\nfour\nfive\n";
        assert_eq!(revert_file(BEFORE, after, after).text, BEFORE);
    }

    #[test]
    fn revert_of_a_revert_redoes() {
        let after = "ONE\ntwo\nthree\nfour\nfive\n";
        let undone = revert_file(BEFORE, after, after).text;
        assert_eq!(revert_file(after, &undone, &undone).text, after);
    }

    #[test]
    fn missing_final_newline_round_trips() {
        let r = revert_file("a\nb", "a\nB", "a\nB");
        assert_eq!(r.text, "a\nb");
    }
}

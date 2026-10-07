//! Reviewer findings + feedback payloads (plan §19).
//!
//! Reviewers are read-only with structured findings; completion gates require
//! evidence. Feedback carries exact review context (path, range, diff,
//! comment, task) so OMP receives precise pointers, not pasted text.

use crate::{FileDiff, Hunk, HunkStatus};
use serde::{Deserialize, Serialize};

/// One reviewer finding (read-only reviewer output).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub path: String,
    /// 0-based start line.
    pub start_line: usize,
    /// Line count.
    pub line_count: usize,
    pub severity: FindingSeverity,
    pub message: String,
}

/// Finding severity. A `Blocker` keeps the review gate unmet (S3 exit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FindingSeverity {
    Info,
    Suggestion,
    Blocker,
}

/// A finding bound to the hunk it names, as the reviewer saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachedFinding {
    pub id: String,
    pub finding: ReviewFinding,
    /// Hunk index when attached (the hunk text, not the index, binds it).
    pub hunk: usize,
    /// The hunk's after-lines when attached. When no hunk of the file has
    /// this text any more, the code the reviewer judged is gone: stale.
    pub hunk_text: String,
    /// `Some(reason)` once a person dismissed it (recorded in the audit log).
    pub dismissed: Option<String>,
}

fn after_text(diff: &FileDiff, hunk: &Hunk) -> String {
    diff.snapshot
        .lines()
        .skip(hunk.after_start)
        .take(hunk.after_count)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Bind `finding` to the unresolved hunk its lines overlap. A finding no
/// hunk covers is refused, naming the hunks it could have meant.
pub fn attach(
    id: &str,
    finding: ReviewFinding,
    diff: &FileDiff,
) -> Result<AttachedFinding, String> {
    let (fs, fe) = (
        finding.start_line,
        finding.start_line + finding.line_count.max(1),
    );
    let open = |s: HunkStatus| !matches!(s, HunkStatus::Accepted | HunkStatus::Rejected);
    let hit = diff.hunks.iter().enumerate().find(|(i, h)| {
        let (hs, he) = (h.after_start, h.after_start + h.after_count.max(1));
        open(diff.statuses[*i]) && fs < he && hs < fe
    });
    match hit {
        Some((i, h)) => Ok(AttachedFinding {
            id: id.to_string(),
            hunk: i,
            hunk_text: after_text(diff, h),
            finding,
            dismissed: None,
        }),
        None => {
            let hunks: Vec<String> = diff
                .hunks
                .iter()
                .enumerate()
                .filter(|(i, _)| open(diff.statuses[*i]))
                .map(|(i, h)| {
                    format!(
                        "hunk {i}: lines {}-{}",
                        h.after_start + 1,
                        h.after_start + h.after_count.max(1)
                    )
                })
                .collect();
            Err(format!(
                "no unresolved hunk in {} at line {}; open hunks: {}",
                diff.path,
                fs + 1,
                if hunks.is_empty() {
                    "none".to_string()
                } else {
                    hunks.join(", ")
                }
            ))
        }
    }
}

impl AttachedFinding {
    /// The hunk the reviewer judged no longer exists in `diff`.
    pub fn is_stale(&self, diff: &FileDiff) -> bool {
        !diff
            .hunks
            .iter()
            .any(|h| after_text(diff, h) == self.hunk_text)
    }

    /// An open, fresh blocker keeps the review gate unmet.
    pub fn blocks(&self, diff: &FileDiff) -> bool {
        self.finding.severity == FindingSeverity::Blocker
            && self.dismissed.is_none()
            && !self.is_stale(diff)
    }

    /// Close it with a reason; once.
    pub fn dismiss(&mut self, reason: &str) -> Result<(), String> {
        if reason.trim().is_empty() {
            return Err("a dismissal needs a reason".to_string());
        }
        if self.dismissed.is_some() {
            return Err(format!("finding {} is already dismissed", self.id));
        }
        self.dismissed = Some(reason.trim().to_string());
        Ok(())
    }
}

/// Structured feedback to OMP (Ask/Fix/Explain + inline comments).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewFeedback {
    pub path: String,
    /// 0-based `(start_line, line_count)`.
    pub range: (usize, usize),
    /// Hunk diff text the comment refers to.
    pub diff: String,
    pub comment: String,
    pub task_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(after: &str, hunks: Vec<(usize, usize)>, statuses: Vec<HunkStatus>) -> FileDiff {
        FileDiff {
            path: "/notes.txt".to_string(),
            hunks: hunks
                .into_iter()
                .map(|(after_start, after_count)| Hunk {
                    before_start: after_start,
                    before_count: 1,
                    after_start,
                    after_count,
                })
                .collect(),
            statuses,
            snapshot: after.to_string(),
        }
    }

    fn finding(start_line: usize, severity: FindingSeverity) -> ReviewFinding {
        ReviewFinding {
            path: "/notes.txt".to_string(),
            start_line,
            line_count: 1,
            severity,
            message: "wrong value".to_string(),
        }
    }

    #[test]
    fn finding_attaches_to_the_pending_hunk_it_names() {
        let d = diff(
            "a\nB\nc\nD\n",
            vec![(1, 1), (3, 1)],
            vec![HunkStatus::Pending; 2],
        );
        let f = attach("f1", finding(3, FindingSeverity::Blocker), &d).unwrap();
        assert_eq!(f.hunk, 1);
        assert_eq!(f.hunk_text, "D");
        assert!(f.blocks(&d), "an open blocker on a pending hunk blocks");
    }

    #[test]
    fn finding_outside_every_hunk_is_refused_with_the_hunks() {
        let d = diff("a\nB\nc\n", vec![(1, 1)], vec![HunkStatus::Pending]);
        let err = attach("f1", finding(2, FindingSeverity::Blocker), &d).unwrap_err();
        assert!(
            err.contains("line 3") && err.contains("hunk 0: lines 2-2"),
            "{err}"
        );
    }

    #[test]
    fn resolved_hunks_take_no_findings() {
        let d = diff("a\nB\n", vec![(1, 1)], vec![HunkStatus::Accepted]);
        assert!(attach("f1", finding(1, FindingSeverity::Blocker), &d).is_err());
    }

    #[test]
    fn blocker_stops_blocking_when_its_hunk_changes_or_it_is_dismissed() {
        let d = diff("a\nB\n", vec![(1, 1)], vec![HunkStatus::Pending]);
        let mut f = attach("f1", finding(1, FindingSeverity::Blocker), &d).unwrap();
        let fixed = diff("a\nB2\n", vec![(1, 1)], vec![HunkStatus::Pending]);
        assert!(f.is_stale(&fixed), "the hunk the reviewer saw is gone");
        assert!(!f.blocks(&fixed));
        f.dismiss("intended: B is the new spelling").unwrap();
        assert!(!f.blocks(&d), "dismissed");
        assert!(f.dismiss("again").is_err(), "a dismissal is recorded once");
        assert!(
            attach("f2", finding(1, FindingSeverity::Suggestion), &d)
                .map(|s| !s.blocks(&d))
                .unwrap(),
            "only blockers block"
        );
    }

    #[test]
    fn dismissal_needs_a_reason() {
        let d = diff("a\nB\n", vec![(1, 1)], vec![HunkStatus::Pending]);
        let mut f = attach("f1", finding(1, FindingSeverity::Blocker), &d).unwrap();
        assert!(f.dismiss("  ").is_err());
    }

    #[test]
    fn feedback_roundtrips() {
        let f = ReviewFeedback {
            path: "/a.rs".to_string(),
            range: (3, 2),
            diff: "-x\n+x\n".to_string(),
            comment: "why?".to_string(),
            task_id: "task-1".to_string(),
        };
        let json = serde_json::to_string(&f).unwrap();
        let back: ReviewFeedback = serde_json::from_str(&json).unwrap();
        assert_eq!(back.range, (3, 2));
    }
}

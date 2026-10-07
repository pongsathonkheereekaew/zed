//! Streaming text batching (§73): `TextDeltaBuffer`.
//!
//! Never repaint per token. Deltas accumulate per `message_id`; the renderer
//! flushes approximately every frame (~16–33ms) and takes the pending text.
//! Crash-during-stream (§74 R3): `discard_all` drops partial buffers — half a
//! message is never rendered as complete.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Flush cadence: every frame-ish. Tests drive `flush_due` with a fake clock
/// via `force_flush`; production calls `flush_due(FRAME_BUDGET)`.
pub const FRAME_BUDGET: Duration = Duration::from_millis(16);

/// Per-message pending text + last-flush time.
#[derive(Debug, Default)]
pub struct TextDeltaBuffer {
    pending: HashMap<String, String>,
    last_flush: Option<Instant>,
}

impl TextDeltaBuffer {
    /// Empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a streaming delta for one message.
    pub fn push(&mut self, message_id: &str, delta: &str) {
        self.pending
            .entry(message_id.to_string())
            .or_default()
            .push_str(delta);
    }

    /// Take all pending text if the frame budget elapsed since the last flush
    /// (or this is the first flush). Returns `message_id → text` drained.
    pub fn flush_due(&mut self, budget: Duration) -> HashMap<String, String> {
        let due = match self.last_flush {
            None => true,
            Some(last) => last.elapsed() >= budget,
        };
        if !due {
            return HashMap::new();
        }
        self.force_flush()
    }

    /// Drain everything now (frame tick, turn end, tests).
    pub fn force_flush(&mut self) -> HashMap<String, String> {
        self.last_flush = Some(Instant::now());
        std::mem::take(&mut self.pending)
    }

    /// Drop all partial text without emitting (disconnect path — §74 R3).
    pub fn discard_all(&mut self) {
        self.pending.clear();
        self.last_flush = None;
    }

    /// Pending message count (for flush scheduling).
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_then_force_flush() {
        let mut b = TextDeltaBuffer::new();
        b.push("m1", "hel");
        b.push("m1", "lo");
        b.push("m2", "x");
        let out = b.force_flush();
        assert_eq!(out.get("m1").map(|s| s.as_str()), Some("hello"));
        assert_eq!(out.get("m2").map(|s| s.as_str()), Some("x"));
        assert_eq!(b.pending_count(), 0);
    }

    #[test]
    fn discard_drops_partial() {
        let mut b = TextDeltaBuffer::new();
        b.push("m1", "half");
        b.discard_all();
        assert!(b.force_flush().is_empty());
    }

    #[test]
    fn second_immediate_flush_not_due() {
        let mut b = TextDeltaBuffer::new();
        b.push("m1", "a");
        assert_eq!(b.flush_due(Duration::from_secs(60)).len(), 1);
        b.push("m1", "b");
        assert!(b.flush_due(Duration::from_secs(60)).is_empty());
    }
}

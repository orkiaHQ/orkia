// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! `Batcher` — accumulates push lines until a flush trigger fires.
//!
//! * `batch_max_events` reached (default 50)
//! * `batch_max_bytes` of serialized JSON (default 256 KB)
//! * `batch_flush_interval` elapsed since the first entry (default 5 s)
//! * shutdown (callers invoke `take()` directly)

use std::time::{Duration, Instant};

use crate::translate::PushLine;

/// Per-chain cursor advance that lands when a batch flushes successfully.
#[derive(Debug, Clone)]
pub struct SealAdvance {
    pub chain_id: String,
    pub last_seq: u64,
    pub last_hash: String,
    pub byte_end: u64,
}

/// One flushable unit.
#[derive(Debug, Default, Clone)]
pub struct Batch {
    lines: Vec<PushLine>,
    seal_advances: Vec<SealAdvance>,
    bytes: usize,
}

impl Batch {
    pub fn lines(&self) -> &[PushLine] {
        &self.lines
    }

    pub fn seal_advances(&self) -> &[SealAdvance] {
        &self.seal_advances
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn add_seal_advance(
        &mut self,
        chain_id: String,
        last_seq: u64,
        last_hash: String,
        byte_end: u64,
        line: Option<PushLine>,
    ) {
        if let Some(l) = line {
            self.bytes += l.serialized_size();
            self.lines.push(l);
        }
        // Coalesce per chain — keep the latest advance only.
        self.seal_advances.retain(|a| a.chain_id != chain_id);
        self.seal_advances.push(SealAdvance {
            chain_id,
            last_seq,
            last_hash,
            byte_end,
        });
    }

    pub fn add_journal(&mut self, line: PushLine) {
        self.bytes += line.serialized_size();
        self.lines.push(line);
    }

    pub fn to_ndjson(&self) -> String {
        self.lines
            .iter()
            .filter_map(|l| match serde_json::to_string(l) {
                Ok(s) => Some(s),
                // The cursor advances over the whole batch on Accept, so a
                // line dropped here would never be re-sent — surface the loss
                // instead of dropping it silently (BUG-099).
                Err(e) => {
                    tracing::warn!(error = %e, "orkia-stream: dropping unserializable batch line");
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub struct Batcher {
    pending: Batch,
    held: Vec<Batch>,
    max_events: usize,
    max_bytes: usize,
    flush_interval: Duration,
    first_event: Option<Instant>,
    /// Max number of held-back batches before we start dropping oldest
    held_cap: usize,
    last_overflow_warn: Option<Instant>,
}

impl Batcher {
    pub fn new(max_events: usize, max_bytes: usize, flush_interval: Duration) -> Self {
        Self {
            pending: Batch::default(),
            held: Vec::new(),
            max_events,
            max_bytes,
            flush_interval,
            first_event: None,
            held_cap: 32,
            last_overflow_warn: None,
        }
    }

    pub fn push_seal(
        &mut self,
        chain_id: &str,
        seq: u64,
        last_hash: String,
        byte_end: u64,
        line: PushLine,
    ) {
        self.first_event.get_or_insert_with(Instant::now);
        self.pending
            .add_seal_advance(chain_id.to_string(), seq, last_hash, byte_end, Some(line));
    }

    /// SEAL event whose cursor must advance even though it was dropped
    /// (private/team/malformed scope).
    pub fn note_dropped_seal(
        &mut self,
        chain_id: &str,
        seq: u64,
        last_hash: String,
        byte_end: u64,
    ) {
        self.pending
            .add_seal_advance(chain_id.to_string(), seq, last_hash, byte_end, None);
    }

    pub fn push_journal(&mut self, line: PushLine) {
        self.first_event.get_or_insert_with(Instant::now);
        self.pending.add_journal(line);
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty() || !self.pending.seal_advances.is_empty() || !self.held.is_empty()
    }

    pub fn should_flush(&self) -> bool {
        if !self.held.is_empty() {
            // Always try to drain held batches first.
            return true;
        }
        if self.pending.is_empty() && self.pending.seal_advances.is_empty() {
            return false;
        }
        if self.pending.lines.len() >= self.max_events {
            return true;
        }
        if self.pending.bytes >= self.max_bytes {
            return true;
        }
        if let Some(first) = self.first_event
            && first.elapsed() >= self.flush_interval
        {
            return true;
        }
        false
    }

    /// Take a batch — held batches come first (FIFO), then the
    /// in-flight pending batch.
    pub fn take(&mut self) -> Batch {
        if !self.held.is_empty() {
            return self.held.remove(0);
        }
        self.first_event = None;
        std::mem::take(&mut self.pending)
    }

    /// Push a failed batch back to the head of the held queue so the
    /// next flush retries it before new events.
    pub fn requeue(&mut self, batch: Batch) {
        if self.held.len() >= self.held_cap {
            // Drop the oldest held batch; emit a rate-limited warning.
            let dropped = self.held.remove(0);
            let now = Instant::now();
            let warn = self
                .last_overflow_warn
                .map(|t| now.duration_since(t) > Duration::from_secs(60))
                .unwrap_or(true);
            if warn {
                self.last_overflow_warn = Some(now);
                tracing::warn!(
                    dropped_lines = dropped.len(),
                    held = self.held.len(),
                    "orkia-stream: held-batch cap reached; dropping oldest batch",
                );
            }
        }
        self.held.insert(0, batch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translate::PushLine;

    fn line(size_hint: usize) -> PushLine {
        let blob = "x".repeat(size_hint);
        PushLine {
            entity_type: "journal_event".into(),
            data: serde_json::json!({"payload": blob}),
        }
    }

    #[test]
    fn flush_by_size_trigger() {
        let mut b = Batcher::new(2, 1_000_000, Duration::from_secs(60));
        b.push_journal(line(10));
        assert!(!b.should_flush());
        b.push_journal(line(10));
        assert!(b.should_flush());
    }

    #[test]
    fn flush_by_byte_trigger() {
        let mut b = Batcher::new(1_000, 50, Duration::from_secs(60));
        b.push_journal(line(100));
        assert!(b.should_flush());
    }

    #[test]
    fn flush_by_time_trigger() {
        let mut b = Batcher::new(1_000, 1_000_000, Duration::from_millis(10));
        b.push_journal(line(10));
        assert!(!b.should_flush());
        std::thread::sleep(Duration::from_millis(20));
        assert!(b.should_flush());
    }

    #[test]
    fn empty_batcher_does_not_flush() {
        let b = Batcher::new(10, 1_000_000, Duration::from_millis(10));
        assert!(!b.should_flush());
    }

    #[test]
    fn take_resets_first_event() {
        let mut b = Batcher::new(10, 1_000_000, Duration::from_millis(10));
        b.push_journal(line(10));
        let batch = b.take();
        assert_eq!(batch.len(), 1);
        assert!(!b.should_flush());
    }

    #[test]
    fn dropped_seal_advances_cursor_without_emitting_line() {
        let mut b = Batcher::new(10, 1_000_000, Duration::from_secs(60));
        b.note_dropped_seal("workspace", 5, "h".into(), 100);
        let batch = b.take();
        assert_eq!(batch.len(), 0);
        assert_eq!(batch.seal_advances().len(), 1);
        assert_eq!(batch.seal_advances()[0].byte_end, 100);
    }

    #[test]
    fn requeue_preserves_fifo() {
        let mut b = Batcher::new(10, 1_000_000, Duration::from_secs(60));
        let mut first = Batch::default();
        first.add_journal(line(10));
        let mut second = Batch::default();
        second.add_journal(line(10));
        b.requeue(second);
        b.requeue(first); // Should land at the head.
        let taken = b.take();
        assert_eq!(taken.len(), 1);
        assert_eq!(b.held.len(), 1);
    }
}

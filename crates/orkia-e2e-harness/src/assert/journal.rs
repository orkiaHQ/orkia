// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Assertions over journal envelopes emitted to `journal.jsonl`.
//!
//! The underlying `JournalTail` accumulates events for the whole
//! session — it has no truncate API. To keep flow-to-flow isolation,
//! each `JournalAssert` carries a `cursor` set by
//! [`crate::OrkiaSession::reset_for_next_flow`]; assertions only see
//! envelopes at index ≥ cursor.

use std::time::{Duration, Instant};

use orkia_test_harness::{JournalEvent, JournalTail};

use crate::error::{AssertKind, HarnessError};

pub struct JournalAssert<'a> {
    tail: Option<&'a JournalTail>,
    /// Index into `tail.all()` below which events are ignored.
    /// Bumped to the current tail length at every flow boundary.
    cursor: usize,
}

impl<'a> JournalAssert<'a> {
    pub fn with_tail(tail: &'a JournalTail, cursor: usize) -> Self {
        Self {
            tail: Some(tail),
            cursor,
        }
    }

    /// No shell was booted — assertions return a clear error.
    pub fn detached() -> Self {
        Self {
            tail: None,
            cursor: 0,
        }
    }

    pub async fn has_envelope(self, kind: &str) -> crate::Result<()> {
        let recent = self.recent_events().await?;
        let count = recent
            .iter()
            .filter(|e| e.event_type() == Some(kind))
            .count();
        if count == 0 {
            let tail = recent_tail_dump(&recent);
            return Err(HarnessError::assertion(
                format!(
                    "journal.has_envelope({kind:?}): none found in {} events since flow start",
                    recent.len()
                ),
                AssertKind::Journal,
                tail,
            ));
        }
        Ok(())
    }

    /// Block until at least one envelope of `kind` appears at index ≥
    /// cursor, or `timeout` elapses. 50 ms poll cadence (matches
    /// `handle_wait`'s shape). Used by F102/F104/F105 to assert on
    /// lifecycle events that arrive asynchronously after a command.
    pub async fn wait_for_envelope(
        self,
        kind: &str,
        timeout: Duration,
    ) -> crate::Result<JournalEvent> {
        let deadline = Instant::now() + timeout;
        let kind_owned = kind.to_string();
        self.wait_for_envelope_with_inner(
            move |e| e.event_type() == Some(&kind_owned),
            deadline,
            &format!("event_type == {kind:?}"),
        )
        .await
    }

    /// Predicate-based variant — block until ANY envelope at index ≥
    /// cursor satisfies `pred`. Used by F105 to filter on
    /// `event:"completed"` with `exit_code != 0`.
    pub async fn wait_for_envelope_with<F>(
        self,
        pred: F,
        timeout: Duration,
        description: &str,
    ) -> crate::Result<JournalEvent>
    where
        F: Fn(&JournalEvent) -> bool + Send + 'static,
    {
        let deadline = Instant::now() + timeout;
        self.wait_for_envelope_with_inner(pred, deadline, description)
            .await
    }

    async fn wait_for_envelope_with_inner<F>(
        &self,
        pred: F,
        deadline: Instant,
        description: &str,
    ) -> crate::Result<JournalEvent>
    where
        F: Fn(&JournalEvent) -> bool,
    {
        let tail = self.tail.ok_or(HarnessError::NotImplemented {
            what: "JournalAssert::wait_for_envelope: shell not booted",
        })?;
        loop {
            let all = tail.all().await;
            if let Some(hit) = all.iter().skip(self.cursor).find(|e| pred(e)) {
                return Ok(hit.clone());
            }
            if Instant::now() >= deadline {
                let all = tail.all().await;
                let recent: Vec<JournalEvent> = all.into_iter().skip(self.cursor).collect();
                return Err(HarnessError::assertion(
                    format!("journal.wait_for_envelope: {description} did not appear in time"),
                    AssertKind::Journal,
                    recent_tail_dump(&recent),
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Count envelopes (at index ≥ cursor) matching the predicate.
    /// Synchronous snapshot — does not wait. Used by F205 to assert
    /// "exactly 20 rfc.ask events for this slug".
    pub async fn count_envelopes_with<F>(self, pred: F) -> crate::Result<usize>
    where
        F: Fn(&JournalEvent) -> bool,
    {
        let tail = self.tail.ok_or(HarnessError::NotImplemented {
            what: "JournalAssert::count_envelopes_with: shell not booted",
        })?;
        let all = tail.all().await;
        Ok(all.into_iter().skip(self.cursor).filter(pred).count())
    }

    /// Return all matching envelopes since cursor, in insertion order.
    pub async fn recent_events_with<F>(self, pred: F) -> crate::Result<Vec<JournalEvent>>
    where
        F: Fn(&JournalEvent) -> bool,
    {
        let tail = self.tail.ok_or(HarnessError::NotImplemented {
            what: "JournalAssert::recent_events_with: shell not booted",
        })?;
        let all = tail.all().await;
        Ok(all.into_iter().skip(self.cursor).filter(pred).collect())
    }

    pub async fn envelope_count(self, kind: &str, expected: usize) -> crate::Result<()> {
        let recent = self.recent_events().await?;
        let got = recent
            .iter()
            .filter(|e| e.event_type() == Some(kind))
            .count();
        if got != expected {
            let tail = recent_tail_dump(&recent);
            return Err(HarnessError::assertion(
                format!(
                    "journal.envelope_count({kind:?}): expected {expected}, got {got} in {} events since flow start",
                    recent.len()
                ),
                AssertKind::Journal,
                tail,
            ));
        }
        Ok(())
    }

    /// Return events at index ≥ cursor, in insertion order.
    async fn recent_events(&self) -> crate::Result<Vec<JournalEvent>> {
        let tail = self.tail.ok_or(HarnessError::NotImplemented {
            what: "JournalAssert: shell not booted",
        })?;
        let all = tail.all().await;
        Ok(all.into_iter().skip(self.cursor).collect())
    }
}

/// Format the last 10 events as a JSONL-ish dump for failure diagnostics.
fn recent_tail_dump(events: &[JournalEvent]) -> String {
    let n = events.len();
    let start = n.saturating_sub(10);
    let mut out = String::from("--- journal events since flow start (last 10) ---\n");
    for e in &events[start..] {
        out.push_str(&serde_json::to_string(&e.raw).unwrap_or_default());
        out.push('\n');
    }
    out
}

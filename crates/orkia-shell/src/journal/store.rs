// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Append-only JSONL store for journal envelopes.
//!
//! V1 is intentionally simple: one envelope per line in
//! `<data_dir>/journal.jsonl`. The store keeps an in-memory `Vec` for
//! fast filtered queries within a session and reloads it from disk on
//! boot. SQLite is deferred until query performance demands it.
//!
//! The file is created by `setup/scaffold.rs` at orkia setup time —
//! `JournalStore::new` only opens/appends.
//!
//! Per ARCHITECTURE invariant #1 (REPL never blocks on I/O other than
//! user input) and #2 (one owner per resource), the on-disk file is
//! owned by a dedicated writer thread. The REPL holds an mpsc sender
//! and the in-memory cache; `append()` is a channel send plus a vector
//! push. The writer keeps the file handle open across appends and only
//! exits when every sender is dropped (i.e. when the REPL shuts down).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread;

use super::types::{JournalEnvelope, JournalFilter};

pub struct JournalStore {
    path: PathBuf,
    entries: Vec<JournalEnvelope>,
    writer_tx: Option<Sender<String>>,
    writer_join: Option<thread::JoinHandle<()>>,
}

impl JournalStore {
    /// Open the store at `<data_dir>/journal.jsonl`, loading any
    /// existing entries into memory. A missing file is treated as an
    /// empty journal — same shape as a fresh `orkia setup`.
    pub fn new(data_dir: &Path) -> Self {
        let path = data_dir.join("journal.jsonl");
        let entries = load(&path);
        let (writer_tx, writer_join) = spawn_writer(path.clone());
        Self {
            path,
            entries,
            writer_tx,
            writer_join,
        }
    }

    /// Append one envelope to the in-memory cache and hand the
    /// serialised line to the writer thread. Returns immediately; the
    /// REPL never waits on disk. Serialise failures and writer-thread
    /// liveness are logged.
    pub fn append(&mut self, envelope: &JournalEnvelope) {
        match serde_json::to_string(envelope) {
            Ok(line) => {
                if let Some(tx) = &self.writer_tx
                    && let Err(e) = tx.send(line)
                {
                    tracing::warn!("journal writer thread is gone; envelope dropped: {e}");
                }
            }
            Err(e) => tracing::warn!("journal serialize failed: {e}"),
        }
        self.entries.push(envelope.clone());
    }

    /// Run a filter against the in-memory cache. Returns results in
    /// insertion order; `last_n` truncates to the most recent N matches.
    pub fn query(&self, filter: &JournalFilter) -> Vec<&JournalEnvelope> {
        let mut hits: Vec<&JournalEnvelope> =
            self.entries.iter().filter(|e| filter.matches(e)).collect();
        if let Some(n) = filter.last_n
            && hits.len() > n
        {
            let drop_count = hits.len() - n;
            hits.drain(..drop_count);
        }
        hits
    }

    /// Like [`Self::query`], but preserves the 1-based global journal index.
    /// Used by navigable citations: `journal://event/N` must resolve to the
    /// same envelope independent of any projection-time filter.
    pub fn query_indexed(&self, filter: &JournalFilter) -> Vec<(usize, &JournalEnvelope)> {
        let mut hits: Vec<(usize, &JournalEnvelope)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| filter.matches(e))
            .map(|(idx, e)| (idx + 1, e))
            .collect();
        if let Some(n) = filter.last_n
            && hits.len() > n
        {
            let drop_count = hits.len() - n;
            hits.drain(..drop_count);
        }
        hits
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load persisted envelopes from `<data_dir>/journal.jsonl` — the on-disk
    /// mirror the writer thread keeps in sync. Used by the migrated `journal`
    /// Command, which reads this mirror via `CommandCtx.data_dir` (the same
    /// `data_dir` mechanism as `history`/`seal`) rather than sharing the
    /// in-memory cache. Eventually-consistent: an envelope still in the writer
    pub fn load_entries(data_dir: &Path) -> Vec<JournalEnvelope> {
        load(&data_dir.join("journal.jsonl"))
    }

    /// Clone of the writer channel — for callers that want to persist
    /// envelopes to disk without going through `append` (which also
    /// updates the in-memory cache).
    ///
    /// The journal listener uses this to tee every envelope it sees to
    /// disk immediately, decoupling on-disk persistence from the REPL's
    /// drain cadence. Returns `None` only if the writer thread failed
    /// to spawn at construction time.
    pub fn writer_handle(&self) -> Option<Sender<String>> {
        self.writer_tx.clone()
    }

    /// Update the in-memory cache only — do NOT send to the writer
    /// thread.
    ///
    /// Used by the REPL drain path after [`Self::writer_handle`] has
    /// already routed disk persistence via the listener's tee. Calling
    /// `append` here would write the same envelope to disk twice.
    pub fn cache_envelope(&mut self, envelope: JournalEnvelope) {
        self.entries.push(envelope);
    }
}

/// Spawn the writer thread that owns the open file handle. Returns
/// `(None, None)` only if the OS refuses the thread, in which case
/// appends degrade to a logged drop — the shell stays alive.
fn spawn_writer(path: PathBuf) -> (Option<Sender<String>>, Option<thread::JoinHandle<()>>) {
    let (tx, rx) = mpsc::channel::<String>();
    let spawn_result = thread::Builder::new()
        .name("orkia-journal-writer".into())
        .spawn(move || writer_loop(path, rx));
    match spawn_result {
        Ok(handle) => (Some(tx), Some(handle)),
        Err(err) => {
            tracing::error!(
                ?err,
                "journal writer spawn failed; on-disk journal disabled this session",
            );
            (None, None)
        }
    }
}

impl Drop for JournalStore {
    fn drop(&mut self) {
        // Drop the sender first so the writer thread sees EOF and
        // finishes draining queued lines, then join so any pending
        // writes hit the disk before the process moves on. Without
        // the join, integration tests that re-open the file
        // immediately after dropping the store would race the writer.
        self.writer_tx.take();
        if let Some(join) = self.writer_join.take() {
            let _ = join.join();
        }
    }
}

fn writer_loop(path: PathBuf, rx: mpsc::Receiver<String>) {
    let mut file = match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "journal writer: open failed; envelopes will be discarded",
            );
            // Drain the channel so senders never block on a full buffer.
            while rx.recv().is_ok() {}
            return;
        }
    };
    while let Ok(line) = rx.recv() {
        if let Err(e) = writeln!(file, "{line}") {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "journal append failed",
            );
        }
    }
}

fn load(path: &Path) -> Vec<JournalEnvelope> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| match serde_json::from_str::<JournalEnvelope>(l) {
            Ok(env) => Some(env),
            Err(e) => {
                tracing::warn!("journal: skipping malformed line: {e}");
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::types::EventType;
    use tempfile::tempdir;

    fn env(event_type: EventType, agent: &str, job_id: u32) -> JournalEnvelope {
        JournalEnvelope {
            event_type,
            timestamp: chrono::Utc::now().to_rfc3339(),
            agent: Some(agent.into()),
            job_id: Some(job_id),
            ..Default::default()
        }
    }

    #[test]
    fn appends_and_reloads_from_disk() {
        let dir = tempdir().expect("tempdir");
        {
            let mut store = JournalStore::new(dir.path());
            store.append(&env(EventType::Hook, "faye", 1));
            store.append(&env(EventType::Lifecycle, "faye", 1));
            assert_eq!(store.len(), 2);
        }
        // New instance loads from disk.
        let store2 = JournalStore::new(dir.path());
        assert_eq!(store2.len(), 2);
    }

    #[test]
    fn hub_seq_round_trips_through_disk_for_backlog_replay() {
        // compute the resubscribe backlog, so the stamped seq MUST survive the
        // disk-tee serialize → reload round-trip.
        let dir = tempdir().expect("tempdir");
        {
            let mut store = JournalStore::new(dir.path());
            let mut e = env(EventType::Hook, "faye", 1);
            e.hub_seq = Some(42);
            store.append(&e);
        }
        let reloaded = JournalStore::load_entries(dir.path());
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].hub_seq, Some(42));

        // The backlog filter the daemon applies (hub_seq > since).
        let since = 40u64;
        let backlog: Vec<_> = reloaded
            .into_iter()
            .filter(|e| e.hub_seq.is_some_and(|s| s > since))
            .collect();
        assert_eq!(backlog.len(), 1);
    }

    #[test]
    fn filter_query_returns_matches() {
        let dir = tempdir().expect("tempdir");
        let mut store = JournalStore::new(dir.path());
        store.append(&env(EventType::Hook, "faye", 1));
        store.append(&env(EventType::Hook, "killua", 2));
        store.append(&env(EventType::Lifecycle, "faye", 1));

        let filter = JournalFilter {
            agent: Some("faye".into()),
            ..Default::default()
        };
        assert_eq!(store.query(&filter).len(), 2);

        let filter = JournalFilter {
            event_type: Some(EventType::Hook),
            ..Default::default()
        };
        assert_eq!(store.query(&filter).len(), 2);

        let filter = JournalFilter {
            agent: Some("faye".into()),
            event_type: Some(EventType::Hook),
            ..Default::default()
        };
        assert_eq!(store.query(&filter).len(), 1);
    }

    #[test]
    fn last_n_keeps_most_recent() {
        let dir = tempdir().expect("tempdir");
        let mut store = JournalStore::new(dir.path());
        for i in 0..5 {
            store.append(&env(EventType::Hook, "faye", i));
        }
        let filter = JournalFilter {
            last_n: Some(2),
            ..Default::default()
        };
        let hits = store.query(&filter);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].job_id, Some(3));
        assert_eq!(hits[1].job_id, Some(4));
    }

    #[test]
    fn skips_malformed_lines_on_load() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("journal.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"hook\",\"timestamp\":\"2026-05-20T10:00:00+00:00\"}\nnot json\n\n",
        )
        .expect("write");
        let store = JournalStore::new(dir.path());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tempdir().expect("tempdir");
        let store = JournalStore::new(dir.path());
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
    }
}

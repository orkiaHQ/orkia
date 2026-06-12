// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Tail `<data_dir>/journal.jsonl` and expose envelopes as a stream.
//!
//! Orkia's `JournalStore` appends every envelope to a single NDJSON
//! file owned by a dedicated writer thread. That file is our stable
//! observation surface — it survives shell crashes, contains every
//! event type (hooks, lifecycle, approval, shell, tell, seal), and
//! never changes ownership across restarts.
//!
//! We parse each line as a permissive `serde_json::Value` rather than
//! the strongly-typed envelope from `orkia-shell-types` so that schema
//! additions don't break the harness while you refactor the shell.
//! Strongly-typed accessors are exposed on [`JournalEvent`] for the
//! common fields.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::wait::WaitError;

/// One parsed line from `journal.jsonl`. Backing JSON is preserved so
/// tests can reach into provider-specific fields with full fidelity.
#[derive(Clone, Debug)]
pub struct JournalEvent {
    pub raw: serde_json::Value,
}

impl JournalEvent {
    pub fn event_type(&self) -> Option<&str> {
        self.raw.get("type").and_then(|v| v.as_str())
    }
    pub fn event(&self) -> Option<&str> {
        self.raw.get("event").and_then(|v| v.as_str())
    }
    pub fn agent(&self) -> Option<&str> {
        self.raw.get("agent").and_then(|v| v.as_str())
    }
    pub fn source(&self) -> Option<&str> {
        self.raw.get("source").and_then(|v| v.as_str())
    }
    pub fn tool(&self) -> Option<&str> {
        self.raw.get("tool").and_then(|v| v.as_str())
    }
    pub fn job_id(&self) -> Option<u64> {
        self.raw.get("job_id").and_then(|v| v.as_u64())
    }
    pub fn timestamp(&self) -> Option<&str> {
        self.raw.get("timestamp").and_then(|v| v.as_str())
    }
    pub fn get(&self, field: &str) -> Option<&serde_json::Value> {
        self.raw.get(field)
    }
}

/// Async tail of `journal.jsonl`. Stores every event ever seen during
/// the test in memory so assertions can run against history without
/// having to subscribe before the event was produced.
#[derive(Clone)]
pub struct JournalTail {
    path: PathBuf,
    events: Arc<Mutex<Vec<JournalEvent>>>,
    _join: Arc<tokio::task::JoinHandle<()>>,
}

impl JournalTail {
    /// Start tailing the given file. The file must already exist
    /// (the sandbox creates it during scaffold). Returns immediately;
    /// reading happens in a background task.
    pub fn start(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        let events: Arc<Mutex<Vec<JournalEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_bg = events.clone();
        let path_bg = path.clone();
        let join = tokio::spawn(async move {
            if let Err(e) = tail_loop(path_bg, events_bg).await {
                tracing::warn!(error = %e, "journal tail loop ended with error");
            }
        });
        Ok(Self {
            path,
            events,
            _join: Arc::new(join),
        })
    }

    /// Snapshot every event seen so far. Cheap; returns a clone of the
    /// in-memory `Vec`.
    pub async fn all(&self) -> Vec<JournalEvent> {
        self.events.lock().await.clone()
    }

    /// Return the first event matching `pred`, or `None`.
    pub async fn find<F>(&self, mut pred: F) -> Option<JournalEvent>
    where
        F: FnMut(&JournalEvent) -> bool,
    {
        self.events.lock().await.iter().find(|e| pred(e)).cloned()
    }

    /// Wait until any event matches `pred` (or timeout). Polled inline
    /// (rather than via `wait_for`) so the predicate doesn't have to
    /// satisfy the `'static` capture rules an async closure would
    /// require.
    pub async fn wait_for_event<F>(
        &self,
        timeout: Duration,
        mut pred: F,
        label: &str,
    ) -> Result<JournalEvent, WaitError>
    where
        F: FnMut(&JournalEvent) -> bool + Send,
    {
        let start = Instant::now();
        loop {
            {
                let g = self.events.lock().await;
                if let Some(e) = g.iter().find(|e| pred(e)) {
                    return Ok(e.clone());
                }
            }
            if start.elapsed() >= timeout {
                return Err(WaitError::Timeout {
                    elapsed: start.elapsed(),
                    context: format!("waiting for event: {label}"),
                });
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Convenience: wait for an envelope with `event_type == "hook"`
    /// and the given hook event name (e.g. "PreToolUse").
    pub async fn wait_for_hook(
        &self,
        event_name: &str,
        timeout: Duration,
    ) -> Result<JournalEvent, WaitError> {
        let name = event_name.to_string();
        self.wait_for_event(
            timeout,
            move |e| e.event_type() == Some("hook") && e.event() == Some(name.as_str()),
            &format!("hook:{event_name}"),
        )
        .await
    }

    /// Convenience: wait for a lifecycle event (e.g. "agent.spawn",
    /// "agent.exit").
    pub async fn wait_for_lifecycle(
        &self,
        event_name: &str,
        timeout: Duration,
    ) -> Result<JournalEvent, WaitError> {
        let name = event_name.to_string();
        self.wait_for_event(
            timeout,
            move |e| e.event_type() == Some("lifecycle") && e.event() == Some(name.as_str()),
            &format!("lifecycle:{event_name}"),
        )
        .await
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

async fn tail_loop(path: PathBuf, events: Arc<Mutex<Vec<JournalEvent>>>) -> anyhow::Result<()> {
    use tokio::fs::File;
    use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader, SeekFrom};

    // Open from position 0. The sandbox-created file is empty, so we
    // start at the very beginning; if any pre-existing content is
    // there we replay it through the in-memory buffer (cheap and
    // useful when the harness is pointed at a non-tempdir for debug).
    let mut file = File::open(&path).await?;
    file.seek(SeekFrom::Start(0)).await?;
    let mut reader = BufReader::new(file);
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf).await?;
        if n == 0 {
            // EOF — wait for the writer to append more. We re-open
            // not strictly needed because we hold an O_RDONLY fd; the
            // writer's appends become visible without truncation.
            tokio::time::sleep(Duration::from_millis(20)).await;
            continue;
        }
        let line = buf.trim_end_matches('\n');
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(raw) => {
                events.lock().await.push(JournalEvent { raw });
            }
            Err(e) => {
                tracing::warn!(error = %e, line = %line, "malformed journal line");
            }
        }
    }
}

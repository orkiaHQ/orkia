// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Tail the per-agent SEAL chain(s) under
//! `<data_dir>/agents/<name>/jobs/<N>/seal.jsonl`.
//!
//! Since the daemon-owned-`@name` flip, a bare `@agent` dispatch spawns
//! a **detached** runtime. A detached runtime keeps its hook envelopes
//! (`PreToolUse` / `PostToolUse` / `cage.verdict`) local to its per-job
//! hub and records them in the per-job SEAL chain; only
//! `AgentFinalResponse` is forwarded up to the daemon's disk-owning hub
//! and thus to the unified `journal.jsonl`. So the durable, observable
//! record of an agent's tool-use hooks is the SEAL chain, NOT the
//! journal — this tail is the assertion surface for those events.
//!
//! Each SEAL line is `{"seq","timestamp","event_type","detail":{…},
//! "hash","prev_hash"}`, e.g.
//! `{"event_type":"hook.PreToolUse","detail":{"tool":"Read",
//! "target":"tmp/a.rs",…}}`. We parse permissively as
//! `serde_json::Value` for the same forward-compat reasons as
//! [`crate::journal`].

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::wait::WaitError;

/// One parsed line from a `seal.jsonl` chain. Backing JSON is preserved
/// so tests can reach provider-specific `detail` fields with full
/// fidelity.
#[derive(Clone, Debug)]
pub struct SealEvent {
    pub raw: serde_json::Value,
}

impl SealEvent {
    /// The SEAL event kind, e.g. `"agent.spawn"`, `"hook.PreToolUse"`,
    /// `"agent.complete"`.
    pub fn event_type(&self) -> Option<&str> {
        self.raw.get("event_type").and_then(|v| v.as_str())
    }

    /// Monotonic per-chain sequence number.
    pub fn seq(&self) -> Option<u64> {
        self.raw.get("seq").and_then(|v| v.as_u64())
    }

    /// A field inside the `detail` object.
    pub fn detail(&self, field: &str) -> Option<&serde_json::Value> {
        self.raw.get("detail").and_then(|d| d.get(field))
    }

    /// `detail.tool` — the tool name on a `hook.*` event.
    pub fn tool(&self) -> Option<&str> {
        self.detail("tool").and_then(|v| v.as_str())
    }

    /// `detail.target` — the tool target (e.g. the file path on a Read).
    pub fn target(&self) -> Option<&str> {
        self.detail("target").and_then(|v| v.as_str())
    }
}

/// On-demand tail of every `seal.jsonl` chain for one agent.
///
/// Reads the (tiny) chain files fresh on each poll rather than holding a
/// background reader — the per-job directory does not exist until the
/// agent spawns, and re-reading a handful of small NDJSON files per poll
/// is cheaper than the dir-watch bookkeeping it would replace.
#[derive(Clone)]
pub struct SealTail {
    /// `<data_dir>/agents/<name>/jobs`.
    jobs_dir: PathBuf,
}

impl SealTail {
    /// Tail the chains for `agent` under `data_dir` (`<home>/.orkia`).
    pub fn for_agent(data_dir: impl AsRef<Path>, agent: &str) -> Self {
        let jobs_dir = data_dir.as_ref().join("agents").join(agent).join("jobs");
        Self { jobs_dir }
    }

    /// Every SEAL event seen so far across all of the agent's jobs, in
    /// (job-dir, seq) order. Missing dir ⇒ empty (agent not spawned yet).
    pub fn all(&self) -> Vec<SealEvent> {
        let mut job_dirs: Vec<PathBuf> = match std::fs::read_dir(&self.jobs_dir) {
            Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
            Err(_) => return Vec::new(),
        };
        job_dirs.sort();
        let mut out = Vec::new();
        for dir in job_dirs {
            let chain = dir.join("seal.jsonl");
            let Ok(text) = std::fs::read_to_string(&chain) else {
                continue;
            };
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(raw) = serde_json::from_str::<serde_json::Value>(line) {
                    out.push(SealEvent { raw });
                }
            }
        }
        out
    }

    /// First event matching `pred`, or `None`.
    pub fn find<F>(&self, mut pred: F) -> Option<SealEvent>
    where
        F: FnMut(&SealEvent) -> bool,
    {
        self.all().into_iter().find(|e| pred(e))
    }

    /// Wait until any SEAL event matches `pred` (or timeout).
    pub async fn wait_for<F>(
        &self,
        timeout: Duration,
        mut pred: F,
        label: &str,
    ) -> Result<SealEvent, WaitError>
    where
        F: FnMut(&SealEvent) -> bool + Send,
    {
        let start = Instant::now();
        loop {
            if let Some(e) = self.all().iter().find(|e| pred(e)) {
                return Ok(e.clone());
            }
            if start.elapsed() >= timeout {
                return Err(WaitError::Timeout {
                    elapsed: start.elapsed(),
                    context: format!("waiting for seal event: {label}"),
                });
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Wait for a `hook.<event_name>` SEAL event (e.g. `"PreToolUse"`).
    pub async fn wait_for_hook(
        &self,
        event_name: &str,
        timeout: Duration,
    ) -> Result<SealEvent, WaitError> {
        let want = format!("hook.{event_name}");
        self.wait_for(timeout, |e| e.event_type() == Some(want.as_str()), &want)
            .await
    }

    /// Wait for an exact `event_type` (e.g. `"agent.spawn"`,
    /// `"agent.complete"`).
    pub async fn wait_for_event_type(
        &self,
        event_type: &str,
        timeout: Duration,
    ) -> Result<SealEvent, WaitError> {
        self.wait_for(timeout, |e| e.event_type() == Some(event_type), event_type)
            .await
    }
}

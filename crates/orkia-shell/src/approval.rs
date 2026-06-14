// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Interactive approval side-channel.
//!
//! Agents running as jobs request approval by writing
//! `approval.request.json` into their per-job run directory
//! (`$ORKIA_RUN_DIR`). A dedicated janitor thread owns all filesystem
//! traffic — directory scans for new requests and `remove_dir_all` on
//! completion. The REPL only ever:
//! 1. Drops the resulting `PendingApproval`s into its in-memory queue,
//! 2. Sends a `Cmd::Scan` with the current active-job snapshot, and
//! 3. Sends `Cmd::Cleanup` when a job exits.
//!
//! This satisfies invariant #1 (no blocking I/O on the REPL thread) and
//! invariant #2 (the run directory has one owner — the janitor).
//! Response writes (`approval.response.json`) happen inline on user
//! resolve because they're driven by an explicit human keypress; the
//! REPL is already idle at that moment.

use orkia_shell_types::JobId;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApprovalRequest {
    pub action: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub risk: Option<String>,
    #[serde(default)]
    pub files_changed: Option<Vec<String>>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResponse {
    pub approved: bool,
    pub responded_at: String,
    pub responded_by: String,
}

/// Where the approval came from. The file path is set for V3
/// file-based approvals; hook-driven approvals (`Hook` source) skip
/// the file path entirely and resolve by writing a keystroke into the
/// agent PTY instead. See `Repl::resolve_approval`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalSource {
    File,
    Hook,
}

#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub job_id: JobId,
    pub request: ApprovalRequest,
    pub request_path: PathBuf,
    pub response_path: PathBuf,
    pub received_at: chrono::DateTime<chrono::Utc>,
    /// Where this approval originated. Lets `resolve` pick the right
    /// path: file write for `File`, PTY keystroke for `Hook`.
    pub source: ApprovalSource,
}

/// Janitor commands.
enum Cmd {
    /// Scan the run-dir entries for the listed active jobs.
    Scan(Vec<JobId>),
    /// Remove the per-job run directory.
    Cleanup(JobId),
}

/// Manages per-job run directories and tracks pending approval requests.
///
/// All filesystem reads happen on the janitor thread (`run_dir` is its
/// owned resource). The REPL holds only the in-memory `pending` queue
/// and the two channel endpoints.
///
/// `janitor_degraded` is set when the background janitor thread failed to
/// spawn. In that state file-based approvals are never discovered.
/// Callers MUST check [`Self::is_degraded`] before spawning agents and
/// surface a warning to the user — agents started in degraded mode run
/// with no approval visibility.
pub struct ApprovalWatcher {
    run_dir: PathBuf,
    pending: Vec<PendingApproval>,
    cmd_tx: Option<Sender<Cmd>>,
    discovery_rx: Option<Receiver<PendingApproval>>,
    janitor_join: Option<thread::JoinHandle<()>>,
    /// True when the janitor thread failed to spawn; approvals will not
    /// be discovered from disk while this flag is set.
    janitor_degraded: bool,
}

impl ApprovalWatcher {
    pub fn new(data_dir: &Path) -> Self {
        let run_dir = data_dir.join("run");
        if let Err(e) = std::fs::create_dir_all(&run_dir) {
            tracing::warn!(error = %e, dir = %run_dir.display(), "approval: failed to create run dir");
        }
        let handles = spawn_janitor(run_dir.clone());
        let janitor_degraded = handles.cmd_tx.is_none();
        Self {
            run_dir,
            pending: Vec::new(),
            cmd_tx: handles.cmd_tx,
            discovery_rx: handles.discovery_rx,
            janitor_join: handles.join,
            janitor_degraded,
        }
    }

    /// Returns `true` when the approval janitor thread failed to spawn.
    ///
    /// In this state file-based approval requests are never discovered from
    /// disk. Callers (e.g. `JobController::spawn`) SHOULD refuse to start
    /// new agents or at minimum surface a visible warning to the user so
    /// approvals are not silently lost.
    pub fn is_degraded(&self) -> bool {
        self.janitor_degraded
    }

    /// Create the run directory for a job. Returns the path to set as
    /// `ORKIA_RUN_DIR` in the spawned job's environment.
    ///
    /// This is called from the `JobController::spawn` path, which runs
    /// off the REPL thread (during `tick`/dispatch). Keeping it inline
    /// is fine — invariant #1 covers the drain prelude, not dispatch.
    pub fn create_job_dir(&self, job_id: JobId) -> PathBuf {
        let dir = self.run_dir.join(job_dir_name(job_id));
        // If this fails the agent gets a non-existent ORKIA_RUN_DIR and its
        // approval writes fail silently; at least surface the cause (BUG-098).
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(error = %e, dir = %dir.display(), "approval: failed to create job run dir");
        }
        dir
    }

    /// Drain any approvals the janitor has discovered, deduplicate
    /// against the existing queue, then enqueue a new scan for the
    /// supplied active jobs. Returns the newly-added pending entries.
    pub fn poll(&mut self, active_job_ids: &[JobId]) -> Vec<PendingApproval> {
        let mut new = Vec::new();
        if let Some(rx) = &self.discovery_rx {
            while let Ok(p) = rx.try_recv() {
                if self.pending.iter().any(|e| e.job_id == p.job_id) {
                    continue;
                }
                new.push(p.clone());
                self.pending.push(p);
            }
        }
        if !active_job_ids.is_empty()
            && let Some(tx) = &self.cmd_tx
            && let Err(e) = tx.send(Cmd::Scan(active_job_ids.to_vec()))
        {
            tracing::warn!("approval janitor is gone: {e}");
        }
        new
    }

    /// Resolve a pending approval. Removes it from the queue and, for
    /// file-sourced approvals, writes `approval.response.json`. Returns
    /// the resolved entry so the caller can perform source-specific
    /// follow-up (e.g. writing `y\n`/`n\n` into the agent PTY for
    /// hook-sourced approvals — done by the REPL, not here, to keep
    /// this module free of PTY plumbing).
    pub fn resolve(
        &mut self,
        job_id: JobId,
        approved: bool,
    ) -> Result<PendingApproval, std::io::Error> {
        let idx = self.pending.iter().position(|p| p.job_id == job_id);
        let Some(idx) = idx else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no pending approval for this job",
            ));
        };
        let pending = self.pending.remove(idx);
        if pending.source == ApprovalSource::File {
            let response = ApprovalResponse {
                approved,
                responded_at: chrono::Utc::now().to_rfc3339(),
                responded_by: "user".into(),
            };
            let json = serde_json::to_string_pretty(&response).map_err(std::io::Error::other)?;
            std::fs::write(&pending.response_path, json)?;
        }
        Ok(pending)
    }

    /// Push a hook-sourced approval into the same pending queue used
    /// by the file path. Returns whether the entry was accepted —
    /// duplicates (same job already pending) are dropped so a chatty
    /// provider firing `PermissionRequest` repeatedly does not stack
    /// up. The caller has already enriched `request` from the hook
    /// payload.
    pub fn push_from_hook(&mut self, job_id: JobId, request: ApprovalRequest) -> bool {
        if self.pending.iter().any(|p| p.job_id == job_id) {
            return false;
        }
        // Hook approvals do not own a request file; we still synthesize
        // a path under run_dir so log/debug output has somewhere to
        // point. `resolve` skips the file write for `Hook` sources.
        let dir = self.run_dir.join(job_dir_name(job_id));
        self.pending.push(PendingApproval {
            job_id,
            request,
            request_path: dir.join("approval.request.json"),
            response_path: dir.join("approval.response.json"),
            received_at: chrono::Utc::now(),
            source: ApprovalSource::Hook,
        });
        true
    }

    /// Drop any in-memory pending entries for the completed job and
    /// hand the directory-removal to the janitor so the REPL never
    /// blocks on `remove_dir_all` (which walks every log file).
    pub fn cleanup_job(&mut self, job_id: JobId) {
        self.pending.retain(|p| p.job_id != job_id);
        if let Some(tx) = &self.cmd_tx
            && let Err(e) = tx.send(Cmd::Cleanup(job_id))
        {
            tracing::warn!(
                job = job_id.0,
                "approval janitor is gone; cleanup deferred: {e}",
            );
        }
    }

    pub fn pending(&self) -> &[PendingApproval] {
        &self.pending
    }

    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }
}

impl Drop for ApprovalWatcher {
    fn drop(&mut self) {
        // Close the command channel so the janitor sees EOF and joins
        // any in-flight directory removal before we exit. Without the
        // join, integration tests that observe `~/.orkia/run/<id>` is
        // gone after `cleanup_job` returns would race the worker.
        self.cmd_tx.take();
        if let Some(join) = self.janitor_join.take() {
            let _ = join.join();
        }
    }
}

struct JanitorHandles {
    cmd_tx: Option<Sender<Cmd>>,
    discovery_rx: Option<Receiver<PendingApproval>>,
    join: Option<thread::JoinHandle<()>>,
}

fn spawn_janitor(run_dir: PathBuf) -> JanitorHandles {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let (discovery_tx, discovery_rx) = mpsc::channel::<PendingApproval>();
    let spawn = thread::Builder::new()
        .name("orkia-approval-janitor".into())
        .spawn(move || janitor_loop(run_dir, cmd_rx, discovery_tx));
    match spawn {
        Ok(handle) => JanitorHandles {
            cmd_tx: Some(cmd_tx),
            discovery_rx: Some(discovery_rx),
            join: Some(handle),
        },
        Err(err) => {
            tracing::error!(
                ?err,
                "approval janitor spawn failed; approvals will be invisible",
            );
            JanitorHandles {
                cmd_tx: None,
                discovery_rx: None,
                join: None,
            }
        }
    }
}

fn janitor_loop(run_dir: PathBuf, cmd_rx: Receiver<Cmd>, discovery_tx: Sender<PendingApproval>) {
    // `seen` lets the janitor avoid re-publishing the same request to
    // the REPL. The REPL also dedups via `pending`, but skipping here
    // means we don't bother re-reading the same JSON every scan.
    let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            Cmd::Scan(jobs) => {
                if scan_jobs(&run_dir, jobs, &mut seen, &discovery_tx).is_err() {
                    return;
                }
            }
            Cmd::Cleanup(job_id) => {
                seen.remove(&job_id.0);
                let dir = run_dir.join(job_dir_name(job_id));
                if let Err(e) = std::fs::remove_dir_all(&dir)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(
                        path = %dir.display(),
                        error = %e,
                        "approval janitor: remove_dir_all failed",
                    );
                }
            }
        }
    }
}

/// Per-job run-dir name component under `<data_dir>/run/`.
///
/// In a detached runtime the daemon-global job id (`ORKIA_DETACHED_JOB_ID`)
/// prefixes the runtime-local id, so two concurrent detached runtimes —
/// each allocating runtime-local ids from 1 — never collide on the same
/// `<data_dir>/run/<name>` directory (which holds the agent's
/// `mcp-config.json`, `context.md`, hooks and approval files). The local
/// suffix keeps it unique if a single runtime ever hosts more than one job.
/// The main REPL has no `ORKIA_DETACHED_JOB_ID`, so its bare-id layout is
/// unchanged. Reading the env here mirrors the recursion guard in
/// `pty_daemon::detached_spawner` — the value is set once at spawn and never
/// mutates, so it is identical across the REPL and janitor threads.
fn job_dir_name(job_id: JobId) -> String {
    match std::env::var("ORKIA_DETACHED_JOB_ID") {
        Ok(daemon) if !daemon.is_empty() => format!("{daemon}-{}", job_id.0),
        _ => format!("{}", job_id.0),
    }
}

/// Scan active job run directories for new approval requests. Deduplicates
/// via `seen`, parses + bounds-checks the JSON, and sends new entries down
/// `discovery_tx`. Returns `Err(())` if the receiver has closed (caller
/// should exit the janitor loop).
fn scan_jobs(
    run_dir: &Path,
    jobs: Vec<JobId>,
    seen: &mut std::collections::HashSet<u32>,
    discovery_tx: &Sender<PendingApproval>,
) -> Result<(), ()> {
    seen.retain(|id| jobs.iter().any(|j| j.0 == *id));
    for job_id in jobs {
        if seen.contains(&job_id.0) {
            continue;
        }
        let dir = run_dir.join(job_dir_name(job_id));
        let request_path = dir.join("approval.request.json");
        let response_path = dir.join("approval.response.json");
        if response_path.exists() || !request_path.exists() {
            continue;
        }
        // The file is written by the agent job (untrusted).
        // Cap the read size + reject oversize payloads so a
        // malicious agent can't OOM the janitor.
        let Ok(content) = read_bounded_string(
            &request_path,
            orkia_shell_types::input_limits::APPROVAL_REQUEST_MAX_BYTES,
        ) else {
            continue;
        };
        let Ok(request) = serde_json::from_str::<ApprovalRequest>(&content) else {
            tracing::warn!(job = job_id.0, "malformed approval.request.json; skipping",);
            continue;
        };
        seen.insert(job_id.0);
        let pending = PendingApproval {
            job_id,
            request,
            request_path,
            response_path,
            received_at: chrono::Utc::now(),
            source: ApprovalSource::File,
        };
        if discovery_tx.send(pending).is_err() {
            return Err(());
        }
    }
    Ok(())
}

/// Read at most `cap` bytes from `path` into a UTF-8 string. Returns
/// `Err(io::Error { kind: InvalidData, ... })` if the file is larger
/// than the cap (rather than truncating silently — partial JSON is
/// meaningless to the parser). Used at the `approval.request.json`
/// trust boundary; the writer is an untrusted agent job.
fn read_bounded_string(path: &Path, cap: usize) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = String::with_capacity(cap.min(8 * 1024));
    let n = file
        .by_ref()
        .take(cap as u64 + 1)
        .read_to_string(&mut buf)?;
    if n > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("approval.request.json exceeds {cap}-byte cap"),
        ));
    }
    Ok(buf)
}

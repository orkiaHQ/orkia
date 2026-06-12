// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Per-scope SEAL chain manager.
//!
//! Replaces the legacy single-chain `SealEmitter`. Owns one chain
//! per active job and one chain per touched project. Job chains
//! live in memory while the job runs and on disk forever; project
//! chains are lazily loaded on first touch and remain cached for
//! the lifetime of the process.
//!
//! The Merkle bridge between scopes is the `job.reference` record:
//! when a job chain closes, its terminal hash (`tip_hash`) is
//! written into the project chain so verifying the project also
//! transitively verifies every referenced job.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use orkia_rfc_core::RfcId;
use orkia_shell_types::job::JobId;

use crate::seal::{SealChain, SealError};

/// Multi-scope SEAL chain manager. Stateless w.r.t. routing —
/// callers decide which `seal_*` method to invoke. Owns the
/// concrete file layout under `data_dir`.
pub struct SealManager {
    /// Job chains keyed by job id. Populated by `create_job_chain`,
    /// closed and evicted on `close_job_chain` / `evict_job_chain`.
    job_chains: HashMap<JobId, SealChain>,
    /// Project chains keyed by project name. Loaded lazily on first
    /// `seal_project` for a given name, then cached.
    project_chains: HashMap<String, SealChain>,
    /// Workspace-level chain. Loaded lazily on first `seal_workspace`.
    /// Used for events that aren't scoped to any single project — e.g.
    workspace_chain: Option<SealChain>,
    /// Base path — usually `~/.orkia`. Job chains live under
    /// `{data_dir}/agents/<agent>/jobs/<id>/seal.jsonl`, project
    /// chains under `{data_dir}/projects/<project>/seal.jsonl`.
    data_dir: PathBuf,
    /// When `Some(...)`, every appended record gets this string
    /// injected as `origin` in its detail JSON. Set from the
    /// `ORKIA_SCHEDULED` env var at construction time — the
    /// `orkia every`-generated crontab line exports it so cron-fired
    /// dispatches show up as `origin: "scheduled"` in the audit
    /// trail without the rest of the codebase needing to know.
    origin_tag: Option<String>,
}

/// Outcome of a deep verification pass. Carries both the project-
/// chain result and a per-referenced-job result so callers can
/// surface which links broke.
pub struct DeepVerifyResult {
    pub project_ok: bool,
    pub project_broken_at: Option<u64>,
    pub project_records: usize,
    pub job_results: Vec<JobVerifyResult>,
}

pub struct JobVerifyResult {
    pub job_id: u32,
    pub agent: String,
    /// Chain hashes link cleanly within the job's chain.
    pub chain_ok: bool,
    /// The tip hash matches what the project chain recorded.
    pub tip_matches: bool,
    pub broken_at: Option<u64>,
    pub record_count: usize,
}

impl SealManager {
    pub fn new(data_dir: PathBuf) -> Self {
        let origin_tag = match std::env::var("ORKIA_SCHEDULED").as_deref() {
            Ok("1") => Some("scheduled".to_string()),
            _ => None,
        };
        Self::with_origin(data_dir, origin_tag)
    }

    /// Test/diagnostic constructor: lets callers force an origin tag
    /// without going through the env var. The env-driven `new()` is
    /// the production path; this exists so unit tests can assert
    /// tagging behaviour without mutating process env.
    pub fn with_origin(data_dir: PathBuf, origin_tag: Option<String>) -> Self {
        Self {
            job_chains: HashMap::new(),
            project_chains: HashMap::new(),
            workspace_chain: None,
            data_dir,
            origin_tag,
        }
    }

    /// True when this manager will tag every record with an `origin`
    /// field. Exposed so the dispatch layer can mirror the same flag
    /// into journal envelopes (which don't go through SealManager).
    pub fn origin_tag(&self) -> Option<&str> {
        self.origin_tag.as_deref()
    }

    /// Merge `origin` into a detail JSON object. No-op when no tag
    /// is set or when `detail` isn't a JSON object (defensive — the
    /// only callers we know about pass objects via `serde_json::json!`).
    fn tag_origin(&self, mut detail: serde_json::Value) -> serde_json::Value {
        if let (Some(origin), Some(obj)) = (self.origin_tag.as_deref(), detail.as_object_mut()) {
            obj.insert(
                "origin".to_string(),
                serde_json::Value::String(origin.to_string()),
            );
        }
        detail
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    // ─── Job chains ────────────────────────────────────────────────

    /// Initialise a fresh job chain. Called at agent spawn. The
    /// returned `&mut SealChain` lets the caller immediately
    /// append the `agent.spawn` genesis record.
    pub fn create_job_chain(&mut self, job_id: JobId, agent_name: &str) -> &mut SealChain {
        let path = self.job_chain_path(agent_name, job_id.0);
        // Genesis means a fresh chain by definition — this is called
        // exactly once per spawn. Job ids restart at 1 per process
        // (REPL relaunch, detached runtime), so a pre-existing file at
        // this path belongs to a PREVIOUS job. Loading it would either
        // splice two jobs into one chain or, if the old job ended on a
        // terminal record, mark the chain closed and silently drop the
        // genesis and every subsequent seal (BUG: pre-2026-06-09 every
        // re-used id audit-blackholed). Archive the old file (it stays
        // complete and independently verifiable) and start empty.
        archive_stale_chain(&path);
        // load_or_quarantine: on I/O failure the returned chain is
        // closed in-memory so subsequent seal_job() calls no-op rather
        // than panicking the shell or corrupting the real file. Audit
        // failures are visible in logs; CLAUDE.md mandates fail-closed
        // for security-critical writes and "never panic in non-test
        // code" — quarantine satisfies both.
        let chain = SealChain::load_or_quarantine(path);
        self.job_chains.entry(job_id).or_insert(chain)
    }

    /// Continue the job chain already on disk (creating it when absent)
    /// WITHOUT archiving. For appenders that build a fresh manager per
    /// event after the genesis happened elsewhere — e.g. the daemon
    /// audit trail: routing those through `create_job_chain` archived
    /// the live chain on every event and reduced the audit to its last
    /// record. A chain that ended on a terminal record loads closed, so
    /// late appends no-op instead of splicing into a finished job.
    pub fn open_job_chain(&mut self, job_id: JobId, agent_name: &str) -> &mut SealChain {
        let path = self.job_chain_path(agent_name, job_id.0);
        let chain = SealChain::load_or_quarantine(path);
        self.job_chains.entry(job_id).or_insert(chain)
    }

    /// Append to a job's chain if one exists. No-op (`Ok(())`) if the
    /// job has no active chain (already closed + evicted, or never
    /// created). Returns `Err(SealError)` when a chain *is* present
    /// and the append itself fails — callers can refuse to emit any
    /// downstream signal that would imply durability.
    pub fn seal_job(
        &mut self,
        job_id: JobId,
        event_type: &str,
        detail: serde_json::Value,
    ) -> Result<(), SealError> {
        self.seal_job_with_rfc(job_id, event_type, detail, None)
    }

    /// Like [`Self::seal_job`] but tags the record with an RFC id so
    /// it contributes to that RFC's SEAL v1 document at closure
    pub fn seal_job_with_rfc(
        &mut self,
        job_id: JobId,
        event_type: &str,
        detail: serde_json::Value,
        rfc_id: Option<RfcId>,
    ) -> Result<(), SealError> {
        let detail = self.tag_origin(detail);
        match self.job_chains.get_mut(&job_id) {
            Some(chain) => {
                // `append_with_rfc` rejects with SealError::Closed once
                // the chain is sealed; we mirror the pre-existing
                // "seal after close is a noop" contract by treating
                // Closed as Ok(()) here — the chain genuinely has no
                // place for the record, and callers expect that.
                match chain.append_with_rfc(event_type, detail, rfc_id) {
                    Ok(_) => Ok(()),
                    Err(SealError::Closed) => Ok(()),
                    Err(e) => Err(e),
                }
            }
            None => Ok(()),
        }
    }

    /// Mark a job chain closed and return its tip hash. The chain
    /// stays in memory until `evict_job_chain` runs — that lets
    /// the project-chain bridge step still read the tip after the
    /// caller's `agent.complete` event has been appended.
    pub fn close_job_chain(&mut self, job_id: JobId) -> Option<String> {
        let chain = self.job_chains.get_mut(&job_id)?;
        chain.close();
        chain.tip_hash().map(|s| s.to_string())
    }

    /// Drop a closed job chain from memory. The on-disk file
    /// persists. Call after `close_job_chain` once the tip hash
    /// has been written into the project chain.
    pub fn evict_job_chain(&mut self, job_id: JobId) {
        self.job_chains.remove(&job_id);
    }

    /// Lookup helper for read-only access to an active chain.
    pub fn job_chain(&self, job_id: JobId) -> Option<&SealChain> {
        self.job_chains.get(&job_id)
    }

    /// Iterate active job chains (running + closed-but-not-evicted).
    pub fn active_job_ids(&self) -> impl Iterator<Item = JobId> + '_ {
        self.job_chains.keys().copied()
    }

    /// Verify a job chain from disk. Loads the file fresh — does
    /// not consult the in-memory cache, so it works for closed +
    /// evicted chains too. Returns `(valid, broken_at, len)`.
    pub fn verify_job(&self, agent_name: &str, job_id: u32) -> (bool, Option<u64>, usize) {
        let path = self.job_chain_path(agent_name, job_id);
        Self::verify_at(&path)
    }

    // ─── Project chains ────────────────────────────────────────────

    /// Append to a project's chain. The project chain is created
    /// lazily on first call. A missing on-disk file is treated as
    /// an empty chain (so a fresh project starts clean).
    pub fn seal_project(
        &mut self,
        project_name: &str,
        event_type: &str,
        detail: serde_json::Value,
    ) -> Result<(), SealError> {
        self.seal_project_with_rfc(project_name, event_type, detail, None)
    }

    /// Like [`Self::seal_project`] but tags the record with an RFC id.
    pub fn seal_project_with_rfc(
        &mut self,
        project_name: &str,
        event_type: &str,
        detail: serde_json::Value,
        rfc_id: Option<RfcId>,
    ) -> Result<(), SealError> {
        let detail = self.tag_origin(detail);
        let chain = self
            .project_chains
            .entry(project_name.to_string())
            .or_insert_with(|| {
                let path = self
                    .data_dir
                    .join("projects")
                    .join(project_name)
                    .join("seal.jsonl");
                // See SealChain::load_or_quarantine: on I/O failure the
                // chain becomes inert in-memory rather than panicking.
                SealChain::load_or_quarantine(path)
            });
        match chain.append_with_rfc(event_type, detail, rfc_id) {
            Ok(_) => Ok(()),
            // A quarantined or sealed project chain swallowed the
            // record by design — same contract as seal_job.
            Err(SealError::Closed) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Convenience wrapper that writes the canonical
    /// `job.reference` record. Called after a job chain closes —
    /// the embedded `job_chain_hash` is the job's `tip_hash`, so
    /// a future deep-verify of the project transitively pins the
    /// job's state.
    pub fn seal_job_reference(
        &mut self,
        project_name: &str,
        job_id: JobId,
        agent_name: &str,
        job_chain_hash: &str,
    ) -> Result<(), SealError> {
        self.seal_project(
            project_name,
            "job.reference",
            serde_json::json!({
                "job_id": job_id.0,
                "agent": agent_name,
                "job_chain_hash": job_chain_hash,
            }),
        )
    }

    pub fn project_chain(&self, project_name: &str) -> Option<&SealChain> {
        self.project_chains.get(project_name)
    }

    // ─── Workspace chain ───────────────────────────────────────────

    /// Append to the workspace-level chain. The chain lives at
    /// `<data_dir>/workspace/seal.jsonl` and is loaded lazily on first
    /// call. Used for events that aren't scoped to any single project
    /// — e.g. `workspace.scope_default_changed`. Same fail-closed
    /// contract as [`seal_project`](Self::seal_project): a quarantined
    /// or sealed chain returns `Ok(())`; a real append failure returns
    /// `Err(SealError)`.
    pub fn seal_workspace(
        &mut self,
        event_type: &str,
        detail: serde_json::Value,
    ) -> Result<(), SealError> {
        self.seal_workspace_with_rfc(event_type, detail, None)
    }

    /// Like [`Self::seal_workspace`] but tags the record with an RFC id.
    pub fn seal_workspace_with_rfc(
        &mut self,
        event_type: &str,
        detail: serde_json::Value,
        rfc_id: Option<RfcId>,
    ) -> Result<(), SealError> {
        let detail = self.tag_origin(detail);
        let chain = self.workspace_chain.get_or_insert_with(|| {
            let path = self.data_dir.join("workspace").join("seal.jsonl");
            SealChain::load_or_quarantine(path)
        });
        match chain.append_with_rfc(event_type, detail, rfc_id) {
            Ok(_) => Ok(()),
            Err(SealError::Closed) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Read-only access to the workspace chain (test helper).
    pub fn workspace_chain(&self) -> Option<&SealChain> {
        self.workspace_chain.as_ref()
    }

    /// Shallow verify of a project chain.
    pub fn verify_project(&self, project_name: &str) -> (bool, Option<u64>, usize) {
        let path = self
            .data_dir
            .join("projects")
            .join(project_name)
            .join("seal.jsonl");
        Self::verify_at(&path)
    }

    /// Deep verify: walks the project chain + each referenced
    /// job chain, and checks the recorded `job_chain_hash`
    /// matches the referenced job's current tip. A flipped tip
    /// after seal (someone tampered with the job chain) breaks
    /// `tip_matches` even if the job's own linkage still passes.
    pub fn verify_project_deep(&self, project_name: &str) -> DeepVerifyResult {
        let project_path = self
            .data_dir
            .join("projects")
            .join(project_name)
            .join("seal.jsonl");

        let project_chain = match SealChain::load(project_path) {
            Ok(c) => c,
            Err(_) => {
                return DeepVerifyResult {
                    project_ok: false,
                    project_broken_at: Some(0),
                    project_records: 0,
                    job_results: Vec::new(),
                };
            }
        };

        let (project_ok, project_broken_at) = project_chain.verify();
        let project_records = project_chain.len();

        let mut job_results = Vec::new();
        for record in project_chain.records() {
            if record.event_type != "job.reference" {
                continue;
            }
            let agent = record
                .detail
                .get("agent")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let job_id = record
                .detail
                .get("job_id")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let expected_hash = record
                .detail
                .get("job_chain_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let job_path = self.job_chain_path(&agent, job_id);
            let job_chain = SealChain::load(job_path).ok();
            let record_count = job_chain.as_ref().map(|c| c.len()).unwrap_or(0);
            let (chain_ok, broken_at) = job_chain
                .as_ref()
                .map(|c| c.verify())
                .unwrap_or((false, Some(0)));
            let tip_matches = job_chain
                .as_ref()
                .and_then(|c| c.tip_hash().map(|h| h == expected_hash))
                .unwrap_or(false);

            job_results.push(JobVerifyResult {
                job_id,
                agent,
                chain_ok,
                tip_matches,
                broken_at,
                record_count,
            });
        }

        DeepVerifyResult {
            project_ok,
            project_broken_at,
            project_records,
            job_results,
        }
    }

    // ─── Helpers ───────────────────────────────────────────────────

    fn job_chain_path(&self, agent_name: &str, job_id: u32) -> PathBuf {
        self.data_dir
            .join("agents")
            .join(agent_name)
            .join("jobs")
            .join(job_id.to_string())
            .join("seal.jsonl")
    }

    /// Common verify-from-disk path used by both `verify_job` and
    /// `verify_project`. Returns `(valid, broken_at, record_count)`.
    fn verify_at(path: &Path) -> (bool, Option<u64>, usize) {
        match SealChain::load(path.to_path_buf()) {
            Ok(chain) => {
                let len = chain.len();
                let (ok, broken) = chain.verify();
                (ok, broken, len)
            }
            Err(_) => (false, Some(0), 0),
        }
    }
}

/// Move a previous job's chain file out of the way before a fresh
/// chain starts at the same path (job ids restart per process, so
/// path re-use is normal). The archive is named `seal.jsonl.N` with
/// the first free `N` — the old chain stays complete and verifiable
/// on its own. Best-effort: on rename failure we log and fall
/// through, where `load_or_quarantine` keeps fail-closed semantics.
fn archive_stale_chain(path: &Path) {
    let non_empty = std::fs::metadata(path)
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    if !non_empty {
        return;
    }
    let Some(archive) = (1..u32::MAX)
        .map(|n| path.with_extension(format!("jsonl.{n}")))
        .find(|p| !p.exists())
    else {
        return;
    };
    if let Err(e) = std::fs::rename(path, &archive) {
        tracing::error!(
            path = %path.display(),
            archive = %archive.display(),
            error = %e,
            "seal: failed to archive previous job chain at genesis",
        );
    } else {
        tracing::info!(
            path = %path.display(),
            archive = %archive.display(),
            "seal: archived previous job chain (re-used job id), starting fresh",
        );
    }
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod tests;

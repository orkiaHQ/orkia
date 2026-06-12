// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Background task that drains the unified `OrkiaEvent` channel
//! and routes each event into the right scoped SEAL chain.
//!
//! Owning model: one task, one `SealManager`. The REPL hands the
//! consumer the `OrkiaEvent` receiver at boot and shares a
//! lock-protected `job_projects` map for project-of-job lookups
//! (populated synchronously at agent spawn before any event for
//! that job can race through the channel).
//!
//! Why a dedicated task instead of inline routing in the REPL: the
//! REPL is parked in `read_line` most of the time. Inline routing
//! would mean SEAL appends only happen between prompts — same bug
//! we already fixed for live toasts and prompt injection. A
//! background task drains the channel continuously so chains land
//! their records in real time.

#[cfg(test)]
#[path = "tests.rs"]
mod tests_mod;

use std::collections::HashMap;
use std::sync::Arc;

use orkia_rfc_core::RfcId;
use orkia_shell_types::job::JobId;
use parking_lot::RwLock;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::protocol::{EventPayload, EventSource, OrkiaEvent};
use crate::seal::pending::ScheduledContext;
use crate::seal::{SealError, SealManager};

/// Shared lookup from job id to project name. Populated by the REPL
/// at agent spawn (synchronously, before the `agent.spawn` event is
/// emitted), read by the consumer when it needs to write a
/// `job.reference` into a project chain.
pub type JobProjects = Arc<RwLock<HashMap<JobId, String>>>;

/// Spawn the consumer onto the current tokio runtime. Owns the
/// `SealManager` for the rest of the process; the only handle the
/// REPL keeps is the shared `JobProjects` map (used to register a
/// job's project membership before spawn). Returns the join handle
/// so tests can await completion if needed.
pub fn spawn(
    rx: UnboundedReceiver<OrkiaEvent>,
    manager: SealManager,
    job_projects: JobProjects,
) -> tokio::task::JoinHandle<()> {
    let sched = ScheduledContext::from_env();
    tokio::spawn(run(rx, manager, job_projects, sched))
}

async fn run(
    mut rx: UnboundedReceiver<OrkiaEvent>,
    mut manager: SealManager,
    job_projects: JobProjects,
    sched: ScheduledContext,
) {
    while let Some(event) = rx.recv().await {
        // Capture identifying fields before consuming the event so
        // the error log can still cite them when `route` returns Err.
        let job_id = event.job_id;
        let agent_name = event.agent_name.clone();
        if let Err(e) = route(&mut manager, &job_projects, &sched, event) {
            // Fail-closed contract: a SEAL append must never silently
            // drop a record. Aborting the consumer would silently
            // break audit for every future job, so the consumer
            // continues — but the failure is logged explicitly with
            // "log error then propagate" form for a background task
            // whose only practical recovery is continuing.
            tracing::error!(
                job_id = job_id.0,
                agent = %agent_name,
                error = %e,
                "seal: append failed — record lost",
            );
        }
    }
    tracing::debug!("seal consumer: channel closed, exiting");
}

/// Dispatch one event. Public so unit tests can drive it without
/// the tokio task; production code calls it via `run`.
pub fn route(
    manager: &mut SealManager,
    job_projects: &JobProjects,
    sched: &ScheduledContext,
    event: OrkiaEvent,
) -> Result<(), SealError> {
    let job_id = event.job_id;
    let agent_name = event.agent_name.clone();
    // Stamp every seal append for this event with the REPL-supplied
    // RFC context. `None` means the event happened outside any RFC
    // scope — its record then hashes identically to a pre-`rfc_id`
    // chain entry (see `chain::compute_hash`).
    let rfc_id = event.rfc_id.clone();
    match event.event {
        // ─── Custom events (REPL-internal emissions) ─────────────
        EventPayload::Custom { name, data } => {
            if name == "agent.spawn" {
                // Genesis of the job chain. `create_job_chain`
                // loads-or-creates; the first append below is the
                // record everyone audits when asking "what
                // instructions did this agent receive?".
                manager.create_job_chain(job_id, &agent_name);
                manager.seal_job_with_rfc(job_id, "agent.spawn", data, rfc_id.clone())?;
            } else if let Some(rest) = name.strip_prefix("rfc.") {
                seal_project_for(
                    manager,
                    job_projects,
                    job_id,
                    &name,
                    data.clone(),
                    rfc_id.clone(),
                )?;
                // Also surface the slug in tracing for debugging.
                let _ = rest;
            } else if name.starts_with("issue.") {
                seal_project_for(manager, job_projects, job_id, &name, data, rfc_id.clone())?;
            } else if name.starts_with("project.") {
                // `project.scope_changed`, etc. The payload carries
                // `project = "<name>"` directly (the REPL knows the
                // project for these mutations); fall through
                // `seal_project_for` which reads that field.
                seal_project_for(manager, job_projects, job_id, &name, data, rfc_id.clone())?;
            } else if name.starts_with("workspace.") {
                // Workspace-level audit events (e.g.
                // `workspace.scope_default_changed`,
                // `workspace.team_joined`) land on the workspace chain.
                manager.seal_workspace_with_rfc(&name, data, rfc_id.clone())?;
            } else if name == "event.redacted" {
                // workspace-level audit events. The chain is never rewritten
                // — this is an additive record; the backend's seal_merge
                // detects it and masks the target's public projection.
                manager.seal_workspace_with_rfc(&name, data, rfc_id.clone())?;
            } else if name.starts_with("reasoning.") {
                // Knowledge the cloud consolidated back into the workspace
                // graph is a workspace-level audit event
                // (`reasoning.nodes_consolidated`).
                // The payload enumerates the node ids so the chain remains the
                // authoritative provenance record for cloud-added knowledge.
                manager.seal_workspace_with_rfc(&name, data, rfc_id.clone())?;
            } else if name == "tell" || name == "approval" || name == "auto_resolve" {
                manager.seal_job_with_rfc(job_id, &name, data, rfc_id.clone())?;
            } else {
                // Unknown custom name — bucket it on the job chain
                // so the record isn't lost. Future expansions can
                // add explicit handlers above.
                manager.seal_job_with_rfc(job_id, &name, data, rfc_id.clone())?;
            }
        }

        // ─── Hook lifecycle ──────────────────────────────────────
        EventPayload::SessionStart { model } => {
            manager.seal_job_with_rfc(
                job_id,
                "hook.SessionStart",
                serde_json::json!({ "model": model }),
                rfc_id.clone(),
            )?;
        }
        EventPayload::SessionEnd { exit_code } => {
            close_job_and_link(
                manager,
                job_projects,
                sched,
                job_id,
                &agent_name,
                exit_code,
                rfc_id.clone(),
            )?;
        }

        // ─── Tool flow ───────────────────────────────────────────
        EventPayload::ToolUse {
            tool,
            target,
            input_summary,
        } => {
            manager.seal_job_with_rfc(
                job_id,
                "hook.PreToolUse",
                serde_json::json!({
                    "tool": tool,
                    "target": target,
                    "input_summary": input_summary,
                }),
                rfc_id.clone(),
            )?;
        }
        EventPayload::ToolResult {
            tool,
            target,
            exit_code,
            output_summary,
        } => {
            manager.seal_job_with_rfc(
                job_id,
                "hook.PostToolUse",
                serde_json::json!({
                    "tool": tool,
                    "target": target,
                    "exit_code": exit_code,
                    "output_summary": output_summary,
                }),
                rfc_id.clone(),
            )?;
        }

        // ─── Permissions ─────────────────────────────────────────
        EventPayload::PermissionRequest {
            tool,
            description,
            risk,
        } => {
            manager.seal_job_with_rfc(
                job_id,
                "hook.PermissionRequest",
                serde_json::json!({
                    "tool": tool,
                    "description": description,
                    "risk": risk,
                }),
                rfc_id.clone(),
            )?;
        }
        EventPayload::PermissionResolved {
            approved,
            resolved_by,
        } => {
            manager.seal_job_with_rfc(
                job_id,
                "approval",
                serde_json::json!({
                    "approved": approved,
                    "resolved_by": resolved_by,
                }),
                rfc_id.clone(),
            )?;
        }

        // ─── State-machine ───────────────────────────────────────
        EventPayload::UserMessage { text } => {
            // Always treat as the byte-injection signal from the
            // state-machine detector. External tells (from peer
            // shells, etc.) come through as Custom `name:"tell"`.
            if matches!(event.source, EventSource::StateMachine) {
                let hash = short_sha(text.as_bytes());
                manager.seal_job_with_rfc(
                    job_id,
                    "prompt_injected",
                    serde_json::json!({
                        "message_hash": hash,
                        "len": text.len(),
                    }),
                    rfc_id.clone(),
                )?;
            }
        }

        // ─── Silently ignore the rest ────────────────────────────
        // PromptStart/Ready/CommandStart/OutputStart/OutputFinished
        // are UI/prompt-cycle signals from OSC 133 — useful for the
        // Surface app but not auditable on their own. Attention is
        // a detector heuristic, not a ground-truth event. The other
        // V2 variants (StateReport, Request, AgentMessage) don't
        // map to existing SEAL events yet.
        EventPayload::PromptStart
        | EventPayload::PromptReady
        | EventPayload::CommandStart { .. }
        | EventPayload::OutputStart
        | EventPayload::OutputFinished { .. }
        | EventPayload::Attention { .. }
        | EventPayload::AgentMessage { .. }
        | EventPayload::StateReport { .. }
        | EventPayload::Request { .. } => {}
    }
    Ok(())
}

/// Common close-and-link path. Appends the terminal record
/// (`agent.complete` / `agent.failed`), closes the chain, reads the
/// tip, and — if the job belongs to a project — writes the
/// cross-scope `job.reference` record.
//
// 7 args is over the project's 4-arg limit, but this is a pre-existing
// internal helper signature (6 args before SEAL v1 per-RFC). Adding the
// rfc_id is the surgical change; refactoring to a config struct is out
// of scope for this PR.
#[allow(clippy::too_many_arguments)]
fn close_job_and_link(
    manager: &mut SealManager,
    job_projects: &JobProjects,
    sched: &ScheduledContext,
    job_id: JobId,
    agent_name: &str,
    exit_code: Option<i32>,
    rfc_id: Option<RfcId>,
) -> Result<(), SealError> {
    let code = exit_code.unwrap_or(0);
    let terminal = if code == 0 {
        "agent.complete"
    } else {
        "agent.failed"
    };

    // Run the seal/link steps, then unconditionally clean up the
    // job state before returning the result. Eviction and the
    // project-association removal are bookkeeping that must run
    // even when persistence failed, otherwise the consumer would
    // keep stale chain handles for a job that's already finished.
    let seal_result: Result<(), SealError> = (|| {
        manager.seal_job_with_rfc(
            job_id,
            terminal,
            serde_json::json!({ "exit_code": exit_code }),
            rfc_id.clone(),
        )?;
        if let Some(tip) = manager.close_job_chain(job_id) {
            let project = job_projects.read().get(&job_id).cloned();
            if let Some(project) = project {
                // Inlined `seal_job_reference` so we can pass rfc_id
                // without exceeding the 4-arg limit on that public
                // method's signature.
                manager.seal_project_with_rfc(
                    &project,
                    "job.reference",
                    serde_json::json!({
                        "job_id": job_id.0,
                        "agent": agent_name,
                        "job_chain_hash": tip,
                    }),
                    rfc_id.clone(),
                )?;
            }
        }
        Ok(())
    })();
    manager.evict_job_chain(job_id);
    // Drop the project association — the job is gone.
    job_projects.write().remove(&job_id);
    seal_result?;

    // crond (`ORKIA_SCHEDULED=1` is exported by the crontab line that
    // `orkia every` writes), parking and notification fall outside
    // the SEAL chain proper. Best-effort writes: failure here doesn't
    // unwind the close above.
    if sched.is_scheduled {
        let data_dir = manager.data_dir().to_path_buf();
        if code != 0 {
            super::pending::append_journal_event(
                &data_dir,
                "lifecycle",
                "scheduled_failure",
                job_id,
                agent_name,
                serde_json::json!({ "exit_code": exit_code }),
            );
        } else if sched.approval_required {
            match super::pending::park_scheduled_result(&data_dir, job_id, agent_name, exit_code) {
                Ok(path) => {
                    super::pending::append_journal_event(
                        &data_dir,
                        "approval",
                        "approval_pending",
                        job_id,
                        agent_name,
                        serde_json::json!({ "pending_path": path }),
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        job = job_id.0,
                        error = %e,
                        "scheduled run: failed to park pending result",
                    );
                }
            }
        }
    }
    Ok(())
}

/// Helper: append to the project chain associated with `job_id`,
/// if any. Used by `rfc.*` and `issue.*` Custom events that arrive
/// through the unified channel.
//
// 6 args is over the project's 4-arg limit, but this is a private
// helper whose argument list grew with the rfc_id thread. Refactoring
// to a config struct is out of scope for this PR.
#[allow(clippy::too_many_arguments)]
fn seal_project_for(
    manager: &mut SealManager,
    job_projects: &JobProjects,
    job_id: JobId,
    event_name: &str,
    data: serde_json::Value,
    rfc_id: Option<RfcId>,
) -> Result<(), SealError> {
    // Two ways the project name might be available:
    // 1. The Custom payload carries it as `data.project` (RFC
    //    builtins know it before any job exists — most RFC mutations
    //    aren't job-scoped). Try that first.
    // 2. Fallback to `job_projects[job_id]` for events emitted by
    //    a delegated agent's flow.
    let project = data
        .get("project")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| job_projects.read().get(&job_id).cloned());

    if let Some(project) = project {
        manager.seal_project_with_rfc(&project, event_name, data, rfc_id)?;
    } else {
        tracing::warn!(
            event = event_name,
            job = job_id.0,
            "seal: project event with no project association; dropping",
        );
    }
    Ok(())
}

fn short_sha(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize()).chars().take(16).collect()
}

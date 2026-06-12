// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

impl Repl {
    pub(crate) fn poll_approvals(&mut self) {
        let active_ids: Vec<JobId> = self
            .jobs
            .list()
            .into_iter()
            .filter(|j| matches!(j.state, JobState::Running | JobState::Foreground))
            .map(|j| j.id)
            .collect();
        let new = self.approvals.poll(&active_ids);
        for pending in new {
            self.attention
                .blocking_approval(crate::attention::BlockingApprovalInput {
                    job_id: pending.job_id,
                    agent: self
                        .jobs
                        .get(pending.job_id)
                        .map(|e| e.label.clone())
                        .unwrap_or_else(|| format!("job:{}", pending.job_id.0)),
                    action: pending.request.action.clone(),
                    risk: pending
                        .request
                        .risk
                        .clone()
                        .unwrap_or_else(|| "unknown".into()),
                });
            self.render_approval_card(&pending);
            self.renderer
                .publish(RenderEvent::Block(BlockContent::SystemInfo(format!(
                    "  use 'orkia approve {0}' or 'orkia deny {0}' to respond",
                    pending.job_id.0
                ))));
        }
    }

    pub(crate) fn render_approval_card(&mut self, pending: &PendingApproval) {
        self.renderer
            .publish(RenderEvent::Block(BlockContent::Approval {
                agent: format!("job:{}", pending.job_id.0),
                action: pending.request.action.clone(),
                risk: pending
                    .request
                    .risk
                    .clone()
                    .unwrap_or_else(|| "unknown".into()),
            }));
        if let Some(desc) = &pending.request.description {
            self.renderer
                .publish(RenderEvent::Block(BlockContent::SystemInfo(format!(
                    "  {desc}"
                ))));
        }
        if let Some(files) = &pending.request.files_changed {
            for f in files {
                self.renderer
                    .publish(RenderEvent::Block(BlockContent::Text(format!("  {f}"))));
            }
        }
    }

    /// Resolve a prompt the detector flagged (claude trust check,
    /// etc.) but for which no file/hook approval exists. Sends a
    /// single keystroke into the agent PTY (`\r` to approve / `q` to
    /// quit) and advances the pending-prompt state machine.
    pub(crate) fn resolve_detector_pending(&mut self, id: JobId, approved: bool) -> Outcome {
        let bytes: &[u8] = if approved { b"\r" } else { b"q" };
        if let Err(e) = self.jobs.write_to_pty(id, bytes) {
            return Outcome::Error(format!("approve: pty write failed: {e}"));
        }
        self.state_machine.on_prompt_resolved(id);
        let agent_name = self
            .jobs
            .get(id)
            .map(|e| e.label.clone())
            .unwrap_or_default();
        self.emit_audit_event(
            id,
            &agent_name,
            "approval",
            serde_json::json!({
                "job_id": id.0,
                "action": "detector.prompt",
                "approved": approved,
            }),
        );
        let mut env = JournalEnvelope::now(EventType::Seal);
        env.job_id = Some(id.0);
        env.event = Some("approval.resolved".into());
        env.action = Some("detector.prompt".into());
        env.source = Some("orkia".into());
        env.description = Some(if approved { "approved" } else { "denied" }.into());
        self.emit_journal(env);
        let verb = if approved { "approved" } else { "denied" };
        Outcome::BuiltinOutput {
            blocks: vec![BlockContent::SystemInfo(format!(
                "✓ [{id}] {verb} (detector prompt)"
            ))],
        }
    }

    pub(crate) fn handle_resolution(&mut self, args: &[String], approved: bool) -> Outcome {
        let pending = self.approvals.pending().to_vec();
        if pending.is_empty() {
            return self.handle_resolution_empty(args, approved);
        }
        let target = match args.first() {
            Some(t) => t,
            None => {
                return Outcome::BuiltinOutput {
                    blocks: list_pending_blocks(&pending),
                };
            }
        };
        let id = match target.parse::<u32>() {
            Ok(n) => JobId(n),
            Err(_) => return Outcome::Error(format!("invalid job id: {target}")),
        };
        let pending_entry = pending.iter().find(|p| p.job_id == id).cloned();
        let action = pending_entry
            .as_ref()
            .map(|p| p.request.action.clone())
            .unwrap_or_default();
        let rfc_metadata = pending_entry
            .as_ref()
            .and_then(|p| p.request.metadata.clone())
            .and_then(|m| m.get("rfc").cloned());
        match self.approvals.resolve(id, approved) {
            Ok(pending) => {
                self.attention.resolve_by_job(id);
                self.emit_resolution(id, approved, &action, rfc_metadata, &pending)
            }
            Err(e) => Outcome::Error(format!("{e}")),
        }
    }

    /// Handle the no-pending-approvals case: fall back to detector if the
    /// target has a pre-init prompt pending; otherwise surface "no pending".
    fn handle_resolution_empty(&mut self, args: &[String], approved: bool) -> Outcome {
        // No file/hook approval, but the prompt detector might
        // be tracking a pre-init prompt (claude trust check,
        // etc.) the user wants to resolve. Try that path before
        // declaring nothing is pending.
        if let Some(id) = parse_resolution_target(args)
            && let Some(state) = self.state_machine.pending_state(id)
            && matches!(
                state,
                crate::terminal_state::PendingState::WaitingForBoot
                    | crate::terminal_state::PendingState::WaitingForApproval
            )
        {
            return self.resolve_detector_pending(id, approved);
        }
        Outcome::BuiltinOutput {
            blocks: vec![BlockContent::SystemInfo("no pending approvals".into())],
        }
    }

    /// Emit audit record, PTY keystroke, journal envelope, and optional
    /// RFC-promote follow-through after a successful `approvals.resolve`.
    fn emit_resolution(
        &mut self,
        id: JobId,
        approved: bool,
        action: &str,
        rfc_metadata: Option<serde_json::Value>,
        pending: &crate::approval::PendingApproval,
    ) -> Outcome {
        let agent_name = self
            .jobs
            .get(id)
            .map(|e| e.label.clone())
            .unwrap_or_default();
        self.emit_audit_event(
            id,
            &agent_name,
            "approval",
            serde_json::json!({
                "job_id": id.0,
                "action": action,
                "approved": approved,
            }),
        );
        // Hook-sourced approvals complete by writing the
        // canonical y/n keystroke into the agent PTY; file-
        // sourced ones are already finished by the response
        // file write in ApprovalWatcher::resolve.
        if pending.source == crate::approval::ApprovalSource::Hook {
            let bytes: &[u8] = if approved { b"y\n" } else { b"n\n" };
            if let Err(e) = self.jobs.write_to_pty(id, bytes) {
                tracing::warn!("approve: pty write failed: {e}");
            }
        }
        // Mirror the resolution into the journal so external
        // queries see it alongside the original request.
        self.emit_journal(resolution_envelope(id, action, approved));
        let rfc_block = self.maybe_rfc_promote(id, approved, action, rfc_metadata);
        let verb = if approved { "approved" } else { "denied" };
        let mut blocks = vec![BlockContent::SystemInfo(format!(
            "✓ [{id}] {verb} · SEAL record created"
        ))];
        if let Some(b) = rfc_block {
            blocks.push(b);
        }
        Outcome::BuiltinOutput { blocks }
    }

    /// RFC-promote follow-through: on accept of an `rfc.promote` approval,
    /// run the queued transition. Returns a result block, or `None` when the
    /// approval is not RFC-promote or the conditions aren't met.
    fn maybe_rfc_promote(
        &mut self,
        id: JobId,
        approved: bool,
        action: &str,
        rfc_metadata: Option<serde_json::Value>,
    ) -> Option<BlockContent> {
        if !approved || action != "rfc.promote" {
            return None;
        }
        let meta = rfc_metadata?;
        let slug = meta.get("slug").and_then(|s| s.as_str())?;
        let project = meta.get("project").and_then(|s| s.as_str())?;
        let p = self.workspace.project(project)?;
        let entry = self.rfc_services.get_or_create(project, &p.path);
        let approver = orkia_rfc_core::AgentId::new("human");
        let rfc_id = orkia_rfc_core::RfcId::new(slug);
        self.emit_audit_event(
            JobId(0),
            "",
            "rfc.approved",
            serde_json::json!({
                "rfc_id": slug,
                "project": project,
                "operation": "promote",
                "approver": "human",
            }),
        );
        let _ = id; // not used by the promote path but keeps the call site consistent
        Some(match entry.service.promote(&rfc_id, &approver) {
            Ok(state) => {
                crate::rfc_state::forward_events(entry.sink.drain(), &self.event_router, project);
                BlockContent::SystemInfo(format!("rfc {slug} promoted: now {state:?}"))
            }
            Err(e) => BlockContent::Error(format!("approve accepted but promote failed: {e}")),
        })
    }
}

/// Build a `Seal` journal envelope recording an approval resolution.
fn resolution_envelope(id: JobId, action: &str, approved: bool) -> JournalEnvelope {
    let mut env = JournalEnvelope::now(EventType::Seal);
    env.job_id = Some(id.0);
    env.event = Some("approval.resolved".into());
    env.action = Some(action.to_string());
    env.source = Some("orkia".into());
    env.description = Some(if approved { "approved" } else { "denied" }.into());
    env
}

/// Build the "list pending approvals" blocks shown when approve/deny is
/// invoked with no target argument.
fn list_pending_blocks(pending: &[PendingApproval]) -> Vec<BlockContent> {
    let mut blocks = vec![BlockContent::SystemInfo(format!(
        "{} pending approval(s)",
        pending.len()
    ))];
    for p in pending {
        blocks.push(BlockContent::Approval {
            agent: format!("job:{}", p.job_id.0),
            action: p.request.action.clone(),
            risk: p.request.risk.clone().unwrap_or_else(|| "unknown".into()),
        });
    }
    blocks.push(BlockContent::SystemInfo(
        "use 'orkia approve <job_id>' or 'orkia deny <job_id>' to respond".into(),
    ));
    blocks
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

/// Inputs for [`Repl::emit_routing_decided_local`], bundled into a struct so
/// the method stays within the 4-argument limit.
pub(crate) struct RoutingDecision<'a> {
    pub project: &'a str,
    pub agent: &'a str,
    pub model: &'a str,
    pub intent: &'a str,
    pub job_uuid: uuid::Uuid,
}

impl Repl {
    /// Push a `<kind>` Custom event into the unified channel and
    /// append a `JournalEnvelope::ScopeChange` to the journal store.
    /// `kind` is one of the `*.scope_set` / `*.scope_changed` /
    /// `workspace.*` strings consumed by `seal::consumer::route` (and
    /// recognised through the `project.*` / `workspace.*` prefix arms
    /// added in PR2). `project` is `None` for workspace-level events.
    pub(crate) fn emit_scope_event_local(
        &mut self,
        kind: &str,
        project: Option<&str>,
        artifact_id: &str,
        previous: Option<orkia_shell_types::Scope>,
        current: orkia_shell_types::Scope,
    ) {
        let actor = "local"; // PR2 stub: real account/agent attribution lands with auth wiring
        let detail = serde_json::json!({
            "kind": kind,
            "artifact_id": artifact_id,
            "previous": previous.map(|s| s.as_str()),
            "current": current.as_str(),
            "actor": actor,
            "project": project,
        });
        self.event_router
            .on_custom(JobId(0), "", kind, detail.clone());

        let mut env = JournalEnvelope::now(EventType::ScopeChange);
        env.source = Some("scope".into());
        env.event = Some(kind.to_string());
        if let Some(p) = project {
            env.target = Some(p.to_string());
        }
        if let Some(obj) = detail.as_object() {
            for (k, v) in obj {
                env.extra.insert(k.clone(), v.clone());
            }
        }
        self.journal_store.append(&env);
    }

    /// Emit an `event.redacted` Custom event (sealed on the workspace chain by
    /// the consumer) plus a live journal envelope. Mirrors
    /// [`Self::emit_scope_event_local`]. `scope=public` so the stream gate
    /// forwards it to the backend.
    pub(crate) fn emit_redact_event_local(&mut self, target_event_id: &str, reason: Option<&str>) {
        let actor = "local"; // matches emit_scope_event_local; real attribution lands with auth wiring
        let detail = serde_json::json!({
            "target_event_id": target_event_id,
            "reason": reason,
            "actor": actor,
            "scope": "public",
        });
        self.event_router
            .on_custom(JobId(0), "", "event.redacted", detail.clone());

        let mut env = JournalEnvelope::now(EventType::Seal);
        env.source = Some("seal".into());
        env.event = Some("event.redacted".into());
        if let Some(obj) = detail.as_object() {
            for (k, v) in obj {
                env.extra.insert(k.clone(), v.clone());
            }
        }
        self.journal_store.append(&env);
    }

    /// The cwd's project name when its effective scope is `public`, else
    /// `None`. Gates all V2 public emission to public-project agent work.
    pub(crate) fn public_project_for_cwd(&self) -> Option<String> {
        let cwd = self.agent_cwd()?;
        let name = self.workspace.resolve_project_name(
            None,
            &cwd,
            self.config.default_project.as_deref(),
        )?;
        (self.resolve_prompt_scope(&cwd) == Some(orkia_shell_types::Scope::Public)).then_some(name)
    }

    /// Emit a `routing.decided` journal event (→ `public_routing_decision`).
    /// forwards it.
    pub(crate) fn emit_routing_decided_local(&mut self, decision: RoutingDecision<'_>) {
        let RoutingDecision {
            project,
            agent,
            model,
            intent,
            job_uuid,
        } = decision;
        let mut env = JournalEnvelope::now(EventType::Lifecycle);
        env.source = Some("orkia".into());
        env.event = Some("routing.decided".into());
        env.target = Some(project.to_string());
        let extra = serde_json::json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "project": project,
            "scope": "public",
            "intent": intent,
            "target_agent_name": agent,
            "target_model": model,
            // Explicit dispatch — heuristic classifier confidence isn't threaded:
            // the legacy per-agent scalar is removed from the
            // wire — it was authority over no decision.
            "confidence": 1.0,
            "job_id": job_uuid.to_string(),
            "routed_at": chrono::Utc::now().to_rfc3339(),
        });
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                env.extra.insert(k.clone(), v.clone());
            }
        }
        self.journal_store.append(&env);
    }

    /// Emit a `job.spawned` journal event (→ `public_job` insert).
    pub(crate) fn emit_public_job_spawned_local(
        &mut self,
        project: &str,
        job_uuid: uuid::Uuid,
        agent: &str,
        model: &str,
        task: &str,
        started_at: &str,
    ) {
        let mut env = JournalEnvelope::now(EventType::Lifecycle);
        env.source = Some("job".into());
        env.event = Some("job.spawned".into());
        env.target = Some(project.to_string());
        let extra = serde_json::json!({
            "id": job_uuid.to_string(),
            "project": project,
            "scope": "public",
            "agent_name": agent,
            "model": model,
            "task_summary": task,
            "status": "running",
            "started_at": started_at,
        });
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                env.extra.insert(k.clone(), v.clone());
            }
        }
        self.journal_store.append(&env);
    }

    /// Emit a `job.complete` journal event (→ `public_job` update).
    pub(crate) fn emit_public_job_complete_local(
        &mut self,
        meta: &PublicJobMeta,
        job_uuid: uuid::Uuid,
        exit_code: i32,
        runtime_ms: i64,
    ) {
        let status = if exit_code == 0 { "complete" } else { "failed" };
        let mut env = JournalEnvelope::now(EventType::Lifecycle);
        env.source = Some("job".into());
        env.event = Some("job.complete".into());
        env.target = Some(meta.project.clone());
        let extra = serde_json::json!({
            "id": job_uuid.to_string(),
            "project": meta.project,
            "scope": "public",
            "agent_name": meta.agent_name,
            "model": meta.model,
            "status": status,
            "started_at": meta.started_at,
            "completed_at": chrono::Utc::now().to_rfc3339(),
            "runtime_ms": runtime_ms,
            "exit_code": exit_code,
        });
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                env.extra.insert(k.clone(), v.clone());
            }
        }
        self.journal_store.append(&env);
    }

    /// stamped with the current `rfc cd` scope, if any. Use this for
    /// every REPL-internal `Custom` payload so events that happen
    /// inside an RFC scope land in that RFC's SEAL v1 slice.
    pub(crate) fn emit_audit_event(
        &self,
        job_id: JobId,
        agent_name: &str,
        name: &str,
        data: serde_json::Value,
    ) {
        let rfc_id = self.rfc_scope.as_ref().map(|s| s.rfc_id.clone());
        self.event_router
            .on_custom_with_rfc(job_id, agent_name, name, data, rfc_id);
    }

    /// Publish a block to the active renderer and remember it in the
    /// `recent_blocks` ring buffer so `tui` can paint context on entry.
    pub(crate) fn emit_block(&mut self, block: BlockContent) {
        if self.recent_blocks.len() == RECENT_BLOCKS_CAP {
            self.recent_blocks.pop_front();
        }
        self.recent_blocks.push_back(block.clone());
        self.renderer.publish(RenderEvent::Block(block));
    }

    pub(crate) fn record_history(&mut self, line: &str, decision: &Decision, mode: &Mode) {
        let (entry_type, agent) = match decision {
            Decision::Shell(_) => (HistoryType::Shell, None),
            Decision::Builtin { name, .. } => {
                let ty = match name.as_str() {
                    "approve" | "deny" => HistoryType::Approval,
                    _ => HistoryType::Builtin,
                };
                (ty, None)
            }
            Decision::Exec(_) => (HistoryType::Builtin, None),
            Decision::Agent { name, .. } => {
                // `@agent` prefix → delegation; heuristic-routed → intent.
                let ty = if matches!(mode, Mode::Agent(_)) {
                    HistoryType::AgentDelegation
                } else {
                    HistoryType::Intent
                };
                (ty, name.clone())
            }
            Decision::Pipeline(_) => (HistoryType::Pipeline, None),
            Decision::ShellToAgent { agent, .. } => {
                (HistoryType::ShellToAgent, Some(agent.clone()))
            }
            Decision::AgentToSink { agent, .. } => (HistoryType::AgentToSink, Some(agent.clone())),
            Decision::NoOp(_) => return,
        };
        self.history.push_typed(entry_type, line, agent, None);
    }
}

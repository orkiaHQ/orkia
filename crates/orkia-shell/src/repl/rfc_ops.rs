// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

impl Repl {
    pub(crate) fn handle_rfc_state(
        &self,
        slug: Option<String>,
        project: Option<String>,
    ) -> Outcome {
        let (entry, project) = match self.rfc_service_addressed(slug.as_deref(), project) {
            Ok(t) => t,
            Err(o) => return o,
        };
        let slug = match slug.or_else(|| self.workspace_default_rfc_slug(&project)) {
            Some(s) => s,
            None => {
                return Outcome::UsageError("usage: rfc state <slug>".into());
            }
        };
        let id = orkia_rfc_core::RfcId::new(slug);
        match entry.service.get_context(&id) {
            Ok(ctx) => Outcome::BuiltinOutput {
                blocks: vec![BlockContent::SystemInfo(format!(
                    "rfc:{} state={:?} v{} hash={} open_ask={} unreviewed={} locked_by={}",
                    ctx.rfc_id,
                    ctx.state,
                    ctx.version,
                    ctx.content_hash,
                    ctx.open_clarifications,
                    ctx.unreviewed_decisions,
                    ctx.locked_by.as_ref().map(|a| a.as_str()).unwrap_or("-"),
                ))],
            },
            Err(e) => Outcome::Error(e.to_string()),
        }
    }

    pub(crate) async fn handle_rfc_transition(
        &mut self,
        slug: Option<String>,
        project: Option<String>,
        op: RfcTransitionOp,
        confirm: bool,
    ) -> Outcome {
        let (entry, project) = match self.rfc_service(project) {
            Ok(t) => t,
            Err(o) => return o,
        };
        let slug = match slug.or_else(|| self.workspace_default_rfc_slug(&project)) {
            Some(s) => s,
            None => {
                return Outcome::UsageError(format!("usage: rfc {} <slug> --yes", op.cli_name()));
            }
        };
        let id = orkia_rfc_core::RfcId::new(slug);
        // we render a preview and refuse — same idiom as `agent remove`.
        // Validation runs *before* gating so unreviewed decisions / wrong
        // state surface immediately rather than after the user types --yes.
        if let Err(e) = entry.service.get_context(&id) {
            return Outcome::Error(e.to_string());
        }
        if !confirm {
            let preview = match &op {
                RfcTransitionOp::Promote => {
                    format!("Promote rfc {id}: DraftActive → Active")
                }
                RfcTransitionOp::Complete => format!("Complete rfc {id}: Active → Completed"),
                RfcTransitionOp::Abandon(reason) => {
                    format!("Abandon rfc {id} (reason: {reason})")
                }
                RfcTransitionOp::Reopen => {
                    format!("Reopen rfc {id}: archive current version, create v+1 in DraftActive")
                }
            };
            return Outcome::BuiltinOutput {
                blocks: vec![
                    BlockContent::SystemInfo(preview),
                    BlockContent::SystemInfo(
                        "re-run with --yes to authorize this transition".into(),
                    ),
                ],
            };
        }
        // Record the explicit human authorization in the SEAL chain *before*
        // the transition so the audit trail captures intent even if the
        // transition itself fails downstream.
        self.event_router.on_custom_with_rfc(
            JobId(0),
            "",
            "rfc.approved",
            serde_json::json!({
                "rfc_id": id.as_str(),
                "project": project,
                "operation": op.cli_name(),
                "approver": "human",
            }),
            Some(id.clone()),
        );
        let actor = orkia_rfc_core::AgentId::new("human");
        let result = match &op {
            RfcTransitionOp::Promote => {
                entry.service.promote(&id, &actor).map(|s| format!("{s:?}"))
            }
            RfcTransitionOp::Complete => entry
                .service
                .complete(&id, &actor)
                .map(|s| format!("{s:?}")),
            RfcTransitionOp::Abandon(reason) => entry
                .service
                .abandon(&id, &actor, reason)
                .map(|s| format!("{s:?}")),
            RfcTransitionOp::Reopen => entry.service.reopen(&id, &actor).map(|s| format!("{s:?}")),
        };
        let events = entry.sink.drain();
        crate::rfc_state::forward_events(events, &self.event_router, &project);
        self.workspace.reload();
        self.emit_workspace_snapshot();
        match result {
            Ok(state) => {
                // kick off SEAL v1 assembly fail-soft — a closure is a
                // business fact, the document is an artefact. Any
                // failure surfaces as a warning, the RFC stays closed,
                // user can retry with `orkia rfc seal <slug>`. No-op
                // unless a `RfcSealAssembler` is wired (proprietary
                // proprietary distribution); OSS builds leave it `None`.
                let assembly_msg: Option<String> = self.maybe_assemble_seal_v1(&id, &op).await;
                let summary = format!(
                    "rfc {} {}: now {state}{}",
                    op.cli_name(),
                    id,
                    assembly_msg.as_deref().unwrap_or("")
                );
                Outcome::BuiltinOutput {
                    blocks: vec![BlockContent::SystemInfo(summary)],
                }
            }
            Err(e) => Outcome::Error(e.to_string()),
        }
    }

    pub(crate) fn handle_rfc_lock_status(
        &self,
        slug: Option<String>,
        project: Option<String>,
    ) -> Outcome {
        let (entry, project) = match self.rfc_service_addressed(slug.as_deref(), project) {
            Ok(t) => t,
            Err(o) => return o,
        };
        let slug = match slug.or_else(|| self.workspace_default_rfc_slug(&project)) {
            Some(s) => s,
            None => return Outcome::UsageError("usage: rfc lock-status <slug>".into()),
        };
        let id = orkia_rfc_core::RfcId::new(slug);
        match entry.service.get_context(&id) {
            Ok(ctx) => Outcome::BuiltinOutput {
                blocks: vec![BlockContent::SystemInfo(match ctx.locked_by {
                    Some(a) => format!("rfc:{} locked by {}", ctx.rfc_id, a),
                    None => format!("rfc:{} unlocked", ctx.rfc_id),
                })],
            },
            Err(e) => Outcome::Error(e.to_string()),
        }
    }

    pub(crate) fn handle_rfc_release_lock(
        &self,
        slug: Option<String>,
        project: Option<String>,
    ) -> Outcome {
        // matter who holds it. The long-lived per-project service cache
        // means the lock store persists across calls, so this actually
        // does something (closes Gap #4 from the verification report).
        let (entry, project) = match self.rfc_service(project) {
            Ok(t) => t,
            Err(o) => return o,
        };
        let slug = match slug.or_else(|| self.workspace_default_rfc_slug(&project)) {
            Some(s) => s,
            None => return Outcome::UsageError("usage: rfc release-lock <slug>".into()),
        };
        let id = orkia_rfc_core::RfcId::new(&slug);
        let released = entry.service.force_release(&id);
        crate::rfc_state::forward_events(entry.sink.drain(), &self.event_router, &project);
        let msg = match released {
            Some(holder) => format!("rfc:{slug} lock force-released (was held by {holder})"),
            None => format!("rfc:{slug} was not locked"),
        };
        Outcome::BuiltinOutput {
            blocks: vec![BlockContent::SystemInfo(msg)],
        }
    }

    /// Resolve the active project and look up its long-lived
    /// `RfcStateService` via the per-project cache (creates one on first
    /// use). Returns `(entry, project_name)` where `entry.service` is the
    /// shared Arc used by both REPL handlers and the MCP dispatcher.
    pub(crate) fn rfc_service(
        &self,
        project: Option<String>,
    ) -> Result<(crate::rfc_state::RfcServiceEntry, String), Outcome> {
        let project = self.resolve_rfc_project(project.as_deref())?;
        let Some(p) = self.workspace.project(&project) else {
            return Err(Outcome::Error(format!("project '{project}' not found")));
        };
        let entry = self.rfc_services.get_or_create(&project, &p.path);
        Ok((entry, project))
    }

    /// Like [`rfc_service`], but for *slug-addressed reads* (`rfc state`,
    /// `rfc lock-status`): when an explicit slug is given and no project
    /// context resolves, the slug drives project resolution via the same
    /// workspace-wide lookup as `rfc show` (fail-closed on collision). With
    /// no slug it falls back to plain project-context resolution, leaving the
    /// caller's `rfc cd` / single-RFC slug defaulting unchanged.
    pub(crate) fn rfc_service_addressed(
        &self,
        slug: Option<&str>,
        project: Option<String>,
    ) -> Result<(crate::rfc_state::RfcServiceEntry, String), Outcome> {
        let project = match slug {
            Some(s) => self.resolve_rfc_target(project.as_deref(), s)?,
            None => self.resolve_rfc_project(project.as_deref())?,
        };
        let Some(p) = self.workspace.project(&project) else {
            return Err(Outcome::Error(format!("project '{project}' not found")));
        };
        let entry = self.rfc_services.get_or_create(&project, &p.path);
        Ok((entry, project))
    }

    /// Slug defaulting order: explicit scope (`rfc cd`) → single-RFC project
    /// → `None`. The caller then surfaces a usage error.
    pub(crate) fn workspace_default_rfc_slug(&self, project: &str) -> Option<String> {
        if let Some(s) = &self.rfc_scope
            && s.project == project
        {
            return Some(s.rfc_id.0.clone());
        }
        let p = self.workspace.project(project)?;
        if p.rfcs.len() == 1 {
            Some(p.rfcs[0].slug.clone())
        } else {
            None
        }
    }

    pub(crate) fn handle_rfc_ask(
        &mut self,
        slug: Option<String>,
        project: Option<String>,
        question: String,
        rationale: String,
    ) -> Outcome {
        let (entry, project) = match self.rfc_service(project) {
            Ok(t) => t,
            Err(o) => return o,
        };
        let slug = match slug.or_else(|| self.workspace_default_rfc_slug(&project)) {
            Some(s) => s,
            None => {
                return Outcome::UsageError("usage: rfc ask <slug> --q ... --rationale ...".into());
            }
        };
        let id = orkia_rfc_core::RfcId::new(slug);
        let req = orkia_rfc_state::AskRequest {
            rfc_id: id.clone(),
            agent: orkia_rfc_core::AgentId::new("human"),
            question,
            rationale,
        };
        let did = match entry.service.ask(req) {
            Ok(d) => d,
            Err(e) => return Outcome::Error(e.to_string()),
        };
        // REPL-local ask: no asking PTY to record. Agents calling via MCP
        // record their own job_id through rfc_pty_bridge.record() at the
        // transport layer.
        crate::rfc_state::forward_events(entry.sink.drain(), &self.event_router, &project);
        Outcome::BuiltinOutput {
            blocks: vec![BlockContent::SystemInfo(format!(
                "clarification {did} opened on rfc {id}"
            ))],
        }
    }

    pub(crate) fn handle_rfc_resolve(
        &mut self,
        slug: Option<String>,
        project: Option<String>,
        decision_id: String,
        answer: String,
    ) -> Outcome {
        let (entry, project) = match self.rfc_service(project) {
            Ok(t) => t,
            Err(o) => return o,
        };
        let slug = match slug.or_else(|| self.workspace_default_rfc_slug(&project)) {
            Some(s) => s,
            None => {
                return Outcome::UsageError("usage: rfc resolve <decision-id> --answer ...".into());
            }
        };
        let id = orkia_rfc_core::RfcId::new(slug);
        let did = orkia_rfc_core::DecisionId::new(decision_id);
        if let Err(e) = entry.service.resolve_clarification(
            &id,
            &did,
            &orkia_rfc_core::AgentId::new("human"),
            &answer,
        ) {
            return Outcome::Error(e.to_string());
        }
        // asked this clarification, deliver the answer back into its
        // stdin so the LLM reader resumes its work. Missing entry just
        // means the ask was REPL-local — silently OK.
        let mut injected_into: Option<JobId> = None;
        if let Some(job_id) = self.rfc_pty_bridge.take(&did) {
            let payload =
                crate::rfc_state::ClarificationPtyBridge::format_resolution(&did, &answer);
            if self.jobs.write_to_pty(job_id, &payload).is_ok() {
                injected_into = Some(job_id);
            }
        }
        crate::rfc_state::forward_events(entry.sink.drain(), &self.event_router, &project);
        let msg = match injected_into {
            Some(j) => format!(
                "resolved {did} on rfc {id}: '{answer}' (injected into job {})",
                j.0
            ),
            None => format!("resolved {did} on rfc {id}: '{answer}'"),
        };
        Outcome::BuiltinOutput {
            blocks: vec![BlockContent::SystemInfo(msg)],
        }
    }
}

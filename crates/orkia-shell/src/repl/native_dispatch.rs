// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Dispatch for `[runtime] type = "native"` agents — the Orkia-owned
//! LLM loop (see [`crate::native`]). Kept out of `agent_dispatch.rs`
//! (already at the module size limit); `dispatch_agent` forks here
//! before vendor command resolution.

use super::*;

use crate::job::NativeRegistration;
use crate::native::NativeSessionMsg;
use crate::native::emit::{GenesisHashes, NativeEmitter};
use crate::native::session::{self, NativeSessionConfig};
use crate::native::tools::ToolExecutor;

impl Repl {
    /// Dispatch `body` to native agent `name`. Mirrors the vendor flow:
    /// live-session reuse first, then the daemon flip (the
    /// detached runtime re-parses the line and lands in the in-process
    /// branch — it has no spawner), then the in-process spawn.
    pub(crate) async fn dispatch_native(
        &mut self,
        name: &str,
        body: &str,
        command_line: Option<&str>,
    ) -> Outcome {
        if let Some(existing) = self.jobs.find_live_native_by_name(name) {
            return self.deliver_to_existing_native(existing, name, body);
        }
        if let Some(line) = command_line
            && let Some(outcome) = self.spawn_detached_for_line(line, name)
        {
            // A native session has no terminal, so the foreground
            // auto-attach that follows a vendor daemon flip must not
            // fire. Swap the `JobSpawned` for guidance — the spawn
            // itself already succeeded; errors pass through unchanged.
            return match outcome {
                Outcome::JobSpawned { job_id, .. } => Outcome::BuiltinOutput {
                    blocks: vec![BlockContent::SystemInfo(format!(
                        "[{}] {name} — native session (daemon-owned, no terminal). \
                         Use `tell {name}`, `journal --agent {name}`, or the final response",
                        job_id.0
                    ))],
                },
                other => other,
            };
        }
        self.spawn_native_in_process(name, body).await
    }

    /// Queue a follow-up body on an already-running native session —
    /// the native analog of `deliver_to_existing_agent`. The actor
    /// runs queued bodies in order, one turn each.
    fn deliver_to_existing_native(
        &mut self,
        job_id: JobId,
        agent_name: &str,
        body: &str,
    ) -> Outcome {
        let body = body.trim();
        if body.is_empty() {
            return Outcome::BuiltinOutput {
                blocks: vec![BlockContent::SystemInfo(format!(
                    "[{}] {} already running — use `tell {}`",
                    job_id.0, agent_name, job_id.0
                ))],
            };
        }
        let delivered = self
            .jobs
            .native_inbound(job_id)
            .is_some_and(|tx| tx.send(NativeSessionMsg::User(body.to_string())).is_ok());
        if !delivered {
            return Outcome::Error(format!("native session [{}] is gone", job_id.0));
        }
        let mut env = JournalEnvelope::now(EventType::Tell);
        env.job_id = Some(job_id.0);
        env.agent = Some(agent_name.to_string());
        env.source = Some("orkia".into());
        env.message = Some(body.to_string());
        self.emit_journal(env);
        Outcome::BuiltinOutput {
            blocks: vec![BlockContent::SystemInfo(format!(
                "  \x1b[36m▸\x1b[0m queued for [{}] \x1b[90m({})\x1b[0m",
                job_id.0, agent_name
            ))],
        }
    }

    /// Spawn the in-process session actor. Fail-closed: no kernel,
    /// no journal, or no final-response surface ⇒ refuse — never a
    /// silent vendor fallback, never an unaudited session.
    async fn spawn_native_in_process(&mut self, name: &str, body: &str) -> Outcome {
        let Some(def) = crate::agent_dir::load_definition_by_name(&self.config.data_dir, name)
        else {
            return Outcome::Error(format!("native: no agent definition found for '{name}'"));
        };
        let orkia_shell_types::AgentRuntimeKind::Native { model } = def.runtime.clone() else {
            return Outcome::Error(format!(
                "native: agent '{name}' is not [runtime] type = \"native\""
            ));
        };
        // Kernel resolution: reuse the classifier's live connection when
        // the plan attached one, else discover the local daemon directly.
        // The adaptive attach is gated on `CognitiveRouting` (a
        // classification capability) — native dispatch is not plan-gated
        // shell-side; the kernel enforces its own gates. Same per-feature
        // discovery as `pipeline_wiring::build`. No kernel ⇒ refuse.
        let kernel = self
            .adaptive_handle
            .as_ref()
            .and_then(|h| h.kernel())
            .or_else(orkia_kernel_client::discover);
        let Some(kernel) = kernel else {
            return Outcome::Error(format!(
                "native: agent '{name}' requires the orkia kernel for model calls \
                 and the kernel is not connected — refusing (no vendor fallback)"
            ));
        };
        let Some(final_response) = self.final_response_service.clone() else {
            return Outcome::Error(
                "native: final-response service unavailable on this REPL; refusing".into(),
            );
        };
        let Some(journal_tx) = self.journal_tx.clone() else {
            return Outcome::Error(
                "native: journal unavailable — refusing an unaudited session".into(),
            );
        };

        let (agent_context, _env, _hooks_provider) = self.build_agent_context(Some(name)).await;
        let tools = self.build_native_tools(name, agent_context.as_ref());

        let (inbound_tx, inbound) = tokio::sync::mpsc::unbounded_channel();
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
        let agent_id = uuid::Uuid::new_v4();
        let job_id = match self.jobs.register_native(NativeRegistration {
            agent_name: name.to_string(),
            agent_id,
            inbound: inbound_tx,
            exit_rx,
        }) {
            Ok(id) => id,
            Err(e) => return Outcome::Error(format!("native: failed to register job: {e}")),
        };

        let emitter = NativeEmitter::new(
            job_id,
            name.to_string(),
            journal_tx,
            self.event_router.clone(),
        );
        if let Some(ctx) = agent_context.as_ref() {
            emitter.genesis(&GenesisHashes {
                system_prompt_hash: ctx.system_prompt_hash(),
                memory_hash: ctx.memory_hash(),
                tools_count: tools.defs().len(),
            });
        }
        self.emit_public_job_on_spawn(job_id, name, body);
        self.record_reasoning_scope(job_id, None).await;

        let initial_body = {
            let trimmed = body.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };
        session::spawn(NativeSessionConfig {
            job_id,
            agent_name: name.to_string(),
            model,
            system_prompt: agent_context.map(|c| c.assembled),
            initial_body,
            kernel,
            tools,
            emitter,
            final_response,
            inbound,
            exit_tx,
        });
        Outcome::JobSpawned {
            job_id,
            foreground: false,
            owner: orkia_shell_types::JobOwner::Local,
        }
    }

    /// Build the session's tool executor — shared with the native
    /// pipeline-stage path (see [`crate::native::tools::build_tool_executor`]).
    /// A missing or unparseable policy.toml means the executor denies
    /// every `shell` call; `recall_knowledge` is offered only when
    /// premium intelligence is active for this context.
    fn build_native_tools(
        &self,
        name: &str,
        context: Option<&crate::agent_context::AgentContext>,
    ) -> ToolExecutor {
        crate::native::tools::build_tool_executor(
            &self.config.data_dir,
            name,
            self.agent_cwd(),
            context.is_some_and(|c| c.knowledge_mcp_bridge),
        )
    }
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

impl Repl {
    /// them into the in-process channel so `drain_job_events` handles them
    /// identically (the acceptance: a pushed event is indistinguishable from an
    /// in-process one). De-dup guard: an event whose job the REPL still owns
    /// and a pushed id colliding with a live in-process id must never tear down
    /// the local job (#2 — one owner). Main REPL/local boot: no receiver, no-op.
    fn inject_pushed_job_events(&mut self) {
        let Some(rx) = self.pushed_job_events.as_mut() else {
            return;
        };
        let mut accepted = Vec::new();
        while let Ok(event) = rx.try_recv() {
            accepted.push(event);
        }
        for event in accepted {
            // Owned in-process ⇒ the in-process path is authoritative; drop the
            // pushed duplicate. Otherwise re-inject onto the in-process channel.
            if self.jobs.get(event.job_id()).is_some() {
                continue;
            }
            let _ = self.jobs.event_tx().send(event);
        }
    }

    pub(crate) fn drain_job_events(&mut self) {
        // Fold daemon-pushed events onto the in-process channel first so the
        // single loop below handles pushed and in-process events uniformly.
        self.inject_pushed_job_events();
        while let Ok(event) = self.job_events.try_recv() {
            // the daemon before any local handling. Observe by reference so the
            // existing in-process drains below run unchanged. Main REPL: no-op.
            if let Some(observer) = &self.job_event_observer {
                observer.on_job_event(&event);
            }
            // Generic per-event SEAL records were dropped — job
            // lifecycle is now sealed via the unified OrkiaEvent
            // path. On `Completed`, we synthesize a `SessionEnd`
            // event so the SEAL consumer closes the job chain
            // (idempotent if a `Stop` hook already triggered the
            // close — see consumer.rs::close_job_and_link).
            if let crate::job::JobEvent::Completed {
                id: job_id,
                exit_code,
                ..
            } = event
            {
                let agent_name = self
                    .jobs
                    .get(job_id)
                    .map(|e| e.label.clone())
                    .or_else(|| self.jobs.native_label(job_id))
                    .unwrap_or_default();
                // public-project agent job (the entry is still live here, so
                // its UUID + start are readable before teardown).
                if let Some(meta) = self.public_job_meta.remove(&job_id)
                    && let Some(job_uuid) = self.jobs.agent_uuid(job_id)
                {
                    let runtime_ms = meta.started.elapsed().as_millis() as i64;
                    self.emit_public_job_complete_local(&meta, job_uuid, exit_code, runtime_ms);
                }
                self.event_router.on_orkia_protocol(
                    job_id,
                    &agent_name,
                    crate::protocol::EventPayload::SessionEnd {
                        exit_code: Some(exit_code),
                    },
                );
                self.approvals.cleanup_job(job_id);
                // Drop the per-job reasoning scope so a later spawn reusing the
                // id can't inherit stale project/RFC attribution (the consumer
                // already closed the session via the synthesized SessionEnd).
                if let Ok(mut map) = self.reasoning_scopes.write() {
                    map.remove(&job_id.0);
                }
                // The agent exited — surface a `prompt_dropped`
                // notification if the user's body was still queued.
                // This stays explicit (not delegated to a hook)
                // because the REPL is the one that owns the toast
                // surface; the StateMachineLifecycle hook only
                // tears down the detector thread.
                if let Some(dropped) = self.state_machine.remove_job(job_id) {
                    self.emit_prompt_dropped(&dropped);
                }
                self.attention.job_ended(job_id);
                // Attachment-driven teardown for everything else
                // (injection executor, SEAL chain, future hooks).
                // The state-machine hook is intentionally a no-op
                // because the explicit `remove_job` above already
                // did its work — see [`crate::job::lifecycle`].
                self.jobs.dispatch_on_complete(job_id, exit_code);
            }
            self.emit_lifecycle_envelope(&event);
            // Suppress the `[N]+ Done`/`Exit N` print if the state-machine
            // worker already surfaced the EXACT line promptly when the
            // agent's engine closed (it only `announced_done` when it knew
            // the reaped code). `remove` also GCs the set. When the worker
            // couldn't reap (unknown code, neutral "exited"), it didn't
            // announce, so this prints the authoritative line.
            let already_announced = if let crate::job::JobEvent::Completed { id, .. } = &event {
                self.announced_done
                    .lock()
                    .map(|mut s| s.remove(id))
                    .unwrap_or(false)
            } else {
                false
            };
            // De-noise consecutive identical lifecycle renders for one job
            // (e.g. a burst of `[1] detached`). Real transitions differ by
            // tag and always render. Terminal events drop their entry so the
            // map stays bounded over a long-lived session.
            let tag = job_event_tag(&event);
            let job = event.job_id();
            let dup = self.last_job_render.get(&job).copied() == Some(tag);
            if matches!(event, crate::job::JobEvent::Completed { .. }) {
                self.last_job_render.remove(&job);
            } else {
                self.last_job_render.insert(job, tag);
            }
            if !already_announced && !dup {
                self.renderer.publish(RenderEvent::JobUpdate(event));
            }
        }
    }

    pub(crate) fn drain_state_machine_events(&mut self) {
        // The worker thread handles live printing; we just consume
        // the forwarded events for journal + PTY-write side effects.
        let mut batch = Vec::new();
        if let Some(rx) = self.state_machine_sideeffect_rx.as_ref() {
            while let Ok(event) = rx.try_recv() {
                batch.push(event);
            }
        }
        if !batch.is_empty() {
            tracing::info!(count = batch.len(), "drain_state_machine_events: draining",);
        }
        for event in batch {
            match event {
                crate::terminal_state::DetectorEvent::Attention(att) => {
                    tracing::info!(
                        job = att.job_id.0,
                        prompt_type = ?att.prompt_type,
                        confidence = att.confidence,
                        "drain: Attention",
                    );
                    self.attention
                        .agent_prompt(crate::attention::AgentPromptInput {
                            job_id: att.job_id,
                            agent: att.agent_name.clone(),
                            summary: format!("prompt detected: {:?}", att.prompt_type),
                            pending_body: att.pending_body_preview.clone(),
                        });
                    self.emit_attention(att);
                }
                crate::terminal_state::DetectorEvent::Injected { job_id, .. } => {
                    // Decision only — the executor is still typing. The
                    // journal `Tell` is deferred to `Delivered` so it
                    // records the real landing time.
                    tracing::debug!(job = job_id.0, "drain: Injected (typing started)");
                }
                crate::terminal_state::DetectorEvent::Delivered {
                    job_id,
                    agent_name,
                    body,
                } => {
                    tracing::info!(
                        job = job_id.0,
                        agent = %agent_name,
                        "drain: Delivered",
                    );
                    self.emit_injection(job_id, &agent_name, &body);
                    self.attention.resolve_by_job(job_id);
                }
                crate::terminal_state::DetectorEvent::Closed { job_id, .. } => {
                    tracing::debug!(job = job_id.0, "drain: Closed");
                }
            }
        }
    }

    /// watcher thread has recompiled. The REPL does only the registry Arc-swap
    /// (the compile + load already happened off-thread); the result surfaces
    /// above the next prompt and in the journal. Drained every tick like the
    /// other event sources — never blocks.
    pub(crate) fn drain_plugin_dev_reloads(&mut self) {
        let mut msgs = Vec::new();
        while let Ok(msg) = self.plugin_dev_rx.try_recv() {
            msgs.push(msg);
        }
        for msg in msgs {
            match msg {
                crate::plugins::DevReloadMsg::Reloaded { name, command } => {
                    let mut reg = (*self.registry).clone();
                    reg.register(std::sync::Arc::new(command));
                    self.registry = std::sync::Arc::new(reg);
                    self.notification_queue
                        .push(format!("plugin `{name}` reloaded (dev)"));
                    let mut env = JournalEnvelope::now(EventType::Shell);
                    env.event = Some("plugin.dev.reload".into());
                    env.source = Some(name);
                    env.message = Some("recompiled and re-registered".into());
                    self.emit_journal(env);
                }
                crate::plugins::DevReloadMsg::Failed { name, error } => {
                    self.notification_queue
                        .push(format!("plugin `{name}` reload failed: {error}"));
                }
            }
        }
    }

    /// Drain envelopes the journal task has received since the last
    /// tick. For each one we:
    ///   * enrich `agent` from `job_id` when missing
    ///   * append to the on-disk store
    ///   * route hook side-effects (PermissionRequest, Stop)
    ///
    /// Rendering of notifications and the SEAL-chain bridge land in
    /// later tasks; this method is the single fan-out point.
    pub(crate) fn drain_journal_events(&mut self) {
        let Some(rx) = self.journal_rx.as_mut() else {
            return;
        };
        let mut batch: Vec<JournalEnvelope> = Vec::new();
        while let Ok(env) = rx.try_recv() {
            batch.push(env);
        }
        if batch.is_empty() {
            return;
        }
        // Build the agent-name lookup ONCE per drain pass instead of
        // re-running `jobs.list()` (which `waitpid`s every entry) for
        // every envelope. With N envelopes and M jobs this turns N*M
        // syscalls per drain into M (audit P2-006).
        let agent_names = self.snapshot_agent_names();
        for mut env in batch {
            self.enrich_envelope(&mut env, &agent_names);
            // Disk persistence already happens via the listener's tee
            // (see `boot_journal`). Here we only update the in-memory
            // cache and route side effects; calling `append` would
            // write the same envelope to disk twice.
            self.journal_store.cache_envelope(env.clone());
            self.attention.observe_hook(&env);
            if env.source.as_deref() == Some("orkia-operator")
                && let Some(line) = crate::journal::notification_for(&env)
            {
                self.notification_queue.push(line);
            }
            self.route_journal_side_effects(&env);
            // Tee into the unified OrkiaEvent stream. V1 adapter:
            // legacy consumers above keep their existing behaviour;
            // new consumers read via `take_orkia_event_rx`.
            self.event_router.on_hook(&env);
        }
    }
}

/// Variant tag of a `JobEvent`, used by `drain_job_events` to de-dup
/// consecutive identical lifecycle renders for the same job.
fn job_event_tag(event: &crate::job::JobEvent) -> &'static str {
    use crate::job::JobEvent::*;
    match event {
        Spawned { .. } => "spawned",
        Attached { .. } => "attached",
        Detached { .. } => "detached",
        Stopped { .. } => "stopped",
        Continued { .. } => "continued",
        Completed { .. } => "completed",
    }
}

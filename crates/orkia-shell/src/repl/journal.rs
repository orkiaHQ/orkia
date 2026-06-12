// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

impl Repl {
    /// Mirror a `JobEvent` into the unified journal as a `Lifecycle`
    /// envelope. Lets external `journal` queries see `spawn`,
    /// `completed`, `stopped`, `continued` alongside agent hook events.
    pub(crate) fn emit_lifecycle_envelope(&self, event: &crate::job::JobEvent) {
        let (label, exit) = match event {
            crate::job::JobEvent::Spawned { .. } => ("spawn", None),
            crate::job::JobEvent::Completed { exit_code, .. } => ("completed", Some(*exit_code)),
            crate::job::JobEvent::Stopped { .. } => ("stopped", None),
            crate::job::JobEvent::Continued { .. } => ("continued", None),
            crate::job::JobEvent::Attached { .. } => ("attached", None),
            crate::job::JobEvent::Detached { .. } => ("detached", None),
        };
        let mut env = JournalEnvelope::now(EventType::Lifecycle);
        env.job_id = Some(event.job_id().0);
        env.event = Some(label.into());
        env.exit_code = exit;
        env.source = Some("orkia".into());
        self.emit_journal(env);
    }

    /// Stand up the unified journal listener. Called once from `run()`
    /// after brush boot. On failure we log and continue with no socket
    /// — agents still spawn, file-based approvals still work; the
    /// only loss is real-time hook notifications.
    pub(crate) fn boot_journal(&mut self, printer: Option<std::sync::mpsc::Sender<String>>) {
        let mcp = crate::rfc_state::McpShellDispatcher::new(
            self.config.data_dir.clone(),
            self.event_router.clone(),
            std::sync::Arc::clone(&self.rfc_pty_bridge),
            std::sync::Arc::clone(&self.rfc_services),
            Some(self.knowledge_activity.clone()),
        );
        // daemon-hosted hub it installs a pump starter. With it, the REPL
        // streams envelopes from the daemon (which owns `orkia.sock`, FRS,
        // and the disk tee) instead of binding the socket itself. Without
        // it, the REPL boots its own local hub (legacy path / fallback).
        if let Some(starter) = self.journal_pump_starter.take() {
            self.boot_journal_subscribed(printer, mcp, starter);
        } else {
            self.boot_journal_local(printer, mcp);
        }
        // Background reaper: scans the per-project service cache every
        // The task is owned by the tokio runtime; dropping the Repl ends
        // it because the cache Arc count drops along with the router.
        crate::rfc_state::spawn_lock_reaper(
            std::sync::Arc::clone(&self.rfc_services),
            self.event_router.clone(),
            std::time::Duration::from_secs(60),
        );
    }

    /// Legacy path: the REPL owns `orkia.sock` and hosts the hub itself
    /// (FRS stop-hook + disk tee included).
    fn boot_journal_local(
        &mut self,
        printer: Option<std::sync::mpsc::Sender<String>>,
        mcp: crate::rfc_state::McpShellDispatcher,
    ) {
        // Construct the journal channel up-front so we can hand `tx`
        // clones to anything that needs to emit envelopes (notably the
        // FinalResponseService). The listener accept loop is bound to
        // the same channel below.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<JournalEnvelope>();
        let handlers = crate::journal::LiveJournalHandlers {
            router: Some(std::sync::Arc::new(self.event_router.clone())
                as std::sync::Arc<dyn crate::journal::HookRouter>),
            printer,
            attach_active: Some(std::sync::Arc::clone(&self.attach_active)),
            mcp: Some(std::sync::Arc::new(mcp)),
            // Stop-hook gets wired post-boot via a bus subscription
            // below — keeps FRS's emit path on the listener side.
            stop_hook: None,
            envelope_hook: self.journal_envelope_hook.clone(),
        };

        // Build FRS + disk-writer handles before hub construction so
        // they are available as config inputs (process-agnostic). FRS must
        // emit into the hub's INGRESS (so its `AgentFinalResponse` hits the
        // broadcast bus → disk tee, detached AFR forwarder), NOT the `tx`
        // drain above — that channel is the post-fanout outbound and skips
        // the bus entirely. The hub doesn't exist yet, so FRS gets a bridge
        // channel relayed into `hub.sender()` after start.
        let (frs_tx, frs_rx) = tokio::sync::mpsc::unbounded_channel::<JournalEnvelope>();
        let stop_hook = self.build_final_response_service(frs_tx);

        // a per-job socket instead of the global `<data_dir>/run/orkia.sock`, and
        // do NOT install the disk tee. The journal store appends to the shared
        // `<data_dir>/journal.jsonl`; a second process teeing into it would be a
        // two-writer race (#2 — one owner per resource). The runtime forwards
        // its events up to the daemon, whose hub owns the disk write.
        let socket_path_override =
            crate::detached_control::detached_runtime_hub_socket(&self.config.data_dir);
        let disk_writer = if socket_path_override.is_some() {
            None
        } else {
            self.journal_store.writer_handle()
        };

        let hub_cfg = crate::journal::JournalHubConfig {
            data_dir: self.config.data_dir.clone(),
            socket_path_override,
            handlers,
            outbound_tx: tx,
            stop_hook,
            disk_writer,
            // Daemon-less fallback / per-job LPH: single-process, no
            seq_seed: None,
        };

        match crate::journal::JournalHub::start(hub_cfg) {
            Ok(hub) => {
                // FRS bridge → ingress: AFR envelopes enter the hub like any
                // socket line (bus fanout, then the drain). FRS's `on_stop`
                // filters on `event == "Stop"`, so no feedback loop.
                spawn_frs_ingress_relay(frs_rx, hub.sender());
                // A caller-installed stop hook (the Team pipeline bundle's
                // FRS) emits through its own channel — relay it into the
                // same ingress so its `AgentFinalResponse` envelopes reach
                // the bus (disk tee, detached AFR forwarder) too. Without
                // this a Team-authed detached runtime captures the turn on
                // disk but never forwards it (no journal row, no sink).
                if let Some(rx) = self.final_response_ingress.take() {
                    spawn_frs_ingress_relay(rx, hub.sender());
                }
                self.journal_tx = Some(hub.sender());
                self.attention.set_journal_sender(Some(hub.sender()));
                self.journal_rx = Some(rx);
                // REPL-resident subscribers: need REPL fields; subscribe to
                // the broadcast bus via the hub before consuming the listener.
                self.spawn_knowledge_activity_subscriber(&hub);
                self.spawn_stream_publisher_hub(&hub);
                // `AgentFinalResponse` envelope up to the daemon so the main
                // REPL's projection/sink sees the turn. Off the listener loop,
                // on the broadcast bus (#1). No-op on the legacy fallback path
                // (no forwarder installed).
                if let Some(fwd) = self.detached_afr_forwarder.clone() {
                    spawn_detached_afr_forwarder(fwd, hub.subscribe());
                }
                let listener = hub.into_listener();
                self.boot_intelligence(&listener);
                self.journal_listener = Some(listener);
            }
            Err(e) => {
                tracing::warn!("journal: listener disabled ({e})");
            }
        }
    }

    /// authoritative hub (FRS + disk tee — they survive a REPL restart). The
    /// REPL runs a non-socket relay fed by `feed_tx`, emits in-process events
    /// UP to the daemon via `emit_rx`, and lets the bin proxy MCP frames to
    /// `mcp`. The three handles are handed to the bin-provided `starter`.
    fn boot_journal_subscribed(
        &mut self,
        printer: Option<std::sync::mpsc::Sender<String>>,
        mcp: crate::rfc_state::McpShellDispatcher,
        starter: crate::repl::JournalPumpStarter,
    ) {
        // Drain channel: the relay feeds REPL-resident handlers + the drain.
        let (drain_tx, drain_rx) = tokio::sync::mpsc::unbounded_channel::<JournalEnvelope>();
        // Emit channel: REPL in-process emits go UP to the daemon (the bin
        // forwards each as a `JournalEmit`). `journal_tx` is the paired sender.
        let (emit_tx, emit_rx) = tokio::sync::mpsc::unbounded_channel::<JournalEnvelope>();

        // Subscribed handlers = the REPL-resident set only. MCP is proxied by
        // the daemon (the dispatcher rides out as the pump's `mcp` handle, not
        // installed here); stop-hook (FRS) + disk tee run daemon-side.
        let handlers = crate::journal::LiveJournalHandlers {
            router: Some(std::sync::Arc::new(self.event_router.clone())
                as std::sync::Arc<dyn crate::journal::HookRouter>),
            printer,
            attach_active: Some(std::sync::Arc::clone(&self.attach_active)),
            mcp: None,
            stop_hook: None,
            envelope_hook: self.journal_envelope_hook.clone(),
        };
        let (hub, feed_tx) = crate::journal::JournalHub::start_relay(handlers, drain_tx);

        self.journal_tx = Some(emit_tx.clone());
        self.attention.set_journal_sender(Some(emit_tx));
        self.journal_rx = Some(drain_rx);

        // Projection capture needs a `FinalResponseSource`; the authoritative
        // FRS is daemon-side, so the REPL source is passive, fed by the
        // streamed `AgentFinalResponse` envelopes (off the REPL drain, via
        // the bus). A caller-installed source (the Team pipeline injects a
        // passive one) is fed the SAME way — in subscribed mode no REPL-side
        // stop hook ever fires, so a source that is not fed here never fires
        // at all (the @a|@b stage waiter would wait out its full timeout).
        let source = match self.final_response_source.clone() {
            Some(installed) => installed,
            None => {
                let streamed = std::sync::Arc::new(
                    crate::repl::streamed_final_response::StreamedFinalResponseSource::default(),
                )
                    as std::sync::Arc<dyn orkia_shell_types::FinalResponseSource>;
                self.final_response_source = Some(streamed.clone());
                streamed
            }
        };
        spawn_streamed_afr_subscriber(source, hub.subscribe());

        self.spawn_knowledge_activity_subscriber(&hub);
        self.spawn_stream_publisher_hub(&hub);
        let listener = hub.into_listener();
        self.boot_intelligence(&listener);
        self.journal_listener = Some(listener);

        // `StreamFrame::JobEvent`s onto this sender; the REPL drains the
        // receiver each loop (de-dup + re-inject — see `drain_job_events`).
        let (pushed_job_tx, pushed_job_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::job::JobEvent>();
        self.pushed_job_events = Some(pushed_job_rx);

        // Hand the pump its handles. The bin owns the daemon connection and
        // drives the reader/writer threads from here on.
        starter(crate::repl::DaemonJournalHandles {
            feed_tx,
            emit_rx,
            mcp: std::sync::Arc::new(mcp),
            job_event_tx: pushed_job_tx,
        });
    }

    /// Build the FinalResponseService unless the caller already installed
    /// one via `with_final_response_hook`. Stores the hook and source on
    /// `self`, then returns the hook Arc for the hub config.
    ///
    /// Takes a `sender` clone so FRS can emit `AgentFinalResponse`
    /// envelopes back through the journal channel (same path as today).
    fn build_final_response_service(
        &mut self,
        sender: tokio::sync::mpsc::UnboundedSender<JournalEnvelope>,
    ) -> Option<std::sync::Arc<dyn orkia_shell_types::JournalStopHook>> {
        if self.final_response_hook.is_some() {
            return self.final_response_hook.clone();
        }
        let service =
            orkia_final_response::FinalResponseService::new(self.config.data_dir.clone(), sender)
                .into_arc();
        // Keep the concrete handle too — native sessions publish their
        // turn outcomes through `publish_native`, which the trait
        // objects below don't expose.
        self.final_response_service = Some(service.clone());
        self.final_response_hook =
            Some(service.clone() as std::sync::Arc<dyn orkia_shell_types::JournalStopHook>);
        if self.final_response_source.is_none() {
            self.final_response_source =
                Some(service as std::sync::Arc<dyn orkia_shell_types::FinalResponseSource>);
        }
        self.final_response_hook.clone()
    }

    fn spawn_knowledge_activity_subscriber(&self, hub: &crate::journal::JournalHub) {
        crate::knowledge_activity::spawn_journal_subscriber(
            self.knowledge_activity.clone(),
            hub.subscribe(),
        );
    }

    /// Boot the Orkia Intelligence reasoning hot path. The consumer subscribes
    /// to the same journal
    /// broadcast bus SEAL and the stream publisher read (no new socket, no new
    /// owner — non-negotiable #2). No-op when no auth provider is wired or the
    /// login + premium gate is closed: the handle stays `None` and enrich at
    /// spawn is an exact pass-through (fail-closed, #8).
    fn boot_intelligence(&mut self, listener: &JournalListener) {
        let Some(auth) = self.auth_provider.clone() else {
            return;
        };
        let mut intel = orkia_kernel::Intelligence::new(auth, None);
        // `identity()` returns `None` unless the gate is open AND the session
        // carries parseable workspace + account UUIDs (fail-closed).
        let Some(identity) = intel.identity() else {
            return;
        };
        let scope = orkia_kernel::CaptureScope {
            workspace_id: identity.workspace_id,
            account_id: identity.account_id,
            // Session-level fallbacks; the live per-job project/RFC is resolved
            // from `reasoning_scopes` (written at spawn — see `agent_dispatch`).
            project_id: None,
            rfc_ref: None,
        };
        // Resolve the cloud base URL once at boot; on a resolution error the
        // sync worker is simply disabled (hot path still captures locally).
        let backend_url = orkia_shell_types::backend::resolve_backend_url(None).unwrap_or_default();
        // SEAL audit sink for cloud-consolidated nodes. Routes
        // `reasoning.nodes_consolidated` through the same EventRouter the rest
        // of the shell seals on — one-way message, never touches the chain.
        let audit = std::sync::Arc::new(crate::reasoning_audit::EventRouterAudit::new(
            self.event_router.clone(),
        )) as std::sync::Arc<dyn orkia_kernel::ReasoningAudit>;
        let cfg = orkia_kernel::BootConfig {
            store_path: self.config.data_dir.join("reasoning").join("reasoning.db"),
            scope,
            bus: listener.subscribe(),
            job_scopes: self.reasoning_scopes.clone(),
            backend_url,
            audit: Some(audit),
        };
        match intel.boot(cfg) {
            Ok(true) => {
                self.intelligence = Some(intel);
                tracing::info!("intelligence: reasoning hot path booted");
            }
            Ok(false) => {
                // Gate closed between `identity()` and `boot()` — stay inert.
            }
            Err(e) => {
                tracing::warn!(error = %e, "intelligence: boot failed; reasoning disabled");
            }
        }
    }

    /// Re-boot intelligence against the live journal listener. Used by
    /// `$reasoning purge` after it tears the consumer down and drops the store.
    /// Takes the listener out of `self` so the `&mut self` boot call can borrow
    /// it, then restores it. No-op when the listener is absent (journal
    /// disabled) or the gate is closed (`boot_intelligence` stays inert).
    pub(crate) fn reboot_intelligence(&mut self) {
        if let Some(listener) = self.journal_listener.take() {
            self.intelligence = None;
            self.boot_intelligence(&listener);
            self.journal_listener = Some(listener);
        }
    }

    /// Hub-taking variant used by `boot_journal`. Subscribes to the hub's
    /// broadcast bus so the stream publisher sees the same envelopes as
    /// the listener-taking variant.
    fn spawn_stream_publisher_hub(&mut self, hub: &crate::journal::JournalHub) {
        let Some(auth) = self.auth_provider.clone() else {
            return;
        };
        let bus = hub.subscribe();
        self.start_stream_publisher(bus, auth);
    }

    /// Shared body for both stream-publisher spawn helpers.
    fn start_stream_publisher(
        &mut self,
        bus: tokio::sync::broadcast::Receiver<JournalEnvelope>,
        auth: std::sync::Arc<dyn orkia_auth::AuthProvider>,
    ) {
        let data_dir = self.config.data_dir.clone();
        let team_cache = self.team_cache.clone();
        let team_probe: orkia_stream::TeamMembershipProbe =
            std::sync::Arc::new(move || team_cache.has_any_team_sync());
        match orkia_stream::StreamConfig::from_env(&data_dir) {
            Ok(cfg) => match orkia_stream::start(cfg, bus, auth, Some(team_probe)) {
                Ok(handle) => {
                    self.stream_handle = handle;
                }
                Err(e) => {
                    tracing::warn!("orkia-stream: failed to start: {e}");
                }
            },
            Err(e) => {
                tracing::warn!("orkia-stream: config error: {e}");
            }
        }
    }

    /// Fill in `agent` from the job table when the envelope only
    /// carries `job_id`. The bridge populates both when env vars are
    /// set, but in-process emits and generic-agent paths can ship
    /// just an id.
    pub(crate) fn enrich_envelope(
        &mut self,
        env: &mut JournalEnvelope,
        agent_names: &std::collections::HashMap<u32, String>,
    ) {
        if env.agent.is_some() {
            return;
        }
        let Some(id) = env.job_id else { return };
        if let Some(name) = agent_names.get(&id) {
            env.agent = Some(name.clone());
        }
    }

    /// Build `{job_id → agent_name}` once for the current drain batch.
    /// Reaps zombies as a side-effect (same as the previous per-call
    /// path) but does it once per batch rather than once per envelope.
    pub(crate) fn snapshot_agent_names(&mut self) -> std::collections::HashMap<u32, String> {
        self.jobs
            .list()
            .into_iter()
            .filter_map(|info| match info.kind {
                JobKind::Agent { agent_name, .. } => Some((info.id.0, agent_name)),
                _ => None,
            })
            .collect()
    }

    /// Side-effects derived from journal events. Today: just trace.
    /// Approval routing, job-stop, SEAL append, and notification
    /// queueing get added in subsequent tasks; this is the single
    /// switchboard they will hang off.
    pub(crate) fn route_journal_side_effects(&mut self, env: &JournalEnvelope) {
        // Toast emission now happens inside the JournalListener's
        // accept loop via `LiveJournalHandlers::printer` — surfacing
        // hook events in real time even while the REPL is parked
        // inside a foreground attach. We deliberately DON'T also
        // push to `notification_queue` here, or the user would see
        // every hook twice (once live, once on the next prompt
        // render). The journal store + SEAL + approval routing
        // below still runs on the REPL drain because those need
        // `&mut self`.
        // SEAL routing is no longer driven from the journal drain
        // — hooks flow through `EventRouter::on_hook` (called from
        // the journal listener task) and the SEAL consumer drains
        // the unified OrkiaEvent channel. The journal store still
        // captures every envelope here for `journal` queries.
        if !matches!(env.event_type, EventType::Hook) {
            return;
        }
        let Some(name) = env.event.as_deref() else {
            return;
        };
        tracing::trace!(
            event = name,
            job_id = env.job_id,
            source = env.source.as_deref(),
            "journal hook"
        );
        // `Stop` reaping is handled by JobController PTY exit
        // detection; the hook arrives slightly before exit and would
        // create a duplicate Completed event if acted on here.
        if name == "PermissionRequest" {
            self.absorb_hook_approval(env);
        }
        // One-shot `-c` mode: the agent's turn just ended. With nothing
        // still queued for it, the dispatched command is complete, so
        // stop the session — its PTY exit then drains as `Completed`,
        // `run_one_command`'s loop sees no live job and returns. This is
        // what makes `orkia --detach -c "@agent …"` reach a terminal
        // state (so `orkia wait` returns `done`) instead of idling as a
        // parked interactive session. Never fires in the interactive REPL
        // (`oneshot_dispatch` is only set by `run_one_command`), where the
        // agent persists for follow-up `tell`s.
        if name == "Stop" && self.oneshot_dispatch {
            self.stop_oneshot_agent(env);
        }
        // `AgentFinalResponse` envelope (clean per-turn text on disk) after each
        // `Stop`. If the job has a sink binding, pipe that text into the sink.
        if name == "AgentFinalResponse" {
            self.write_agent_sink(env);
        }
    }

    /// One-shot `-c` teardown: a `Stop` hook means the agent finished a turn.
    /// If no body is still queued for that job, the dispatched command has
    /// delivered its single turn, so stop the session (SIGTERM via
    /// [`JobController::stop`] — the same teardown the `--once` sink uses). The
    /// PTY exit reaps as `Completed`, `run_one_command`'s poll loop then sees no
    /// live job and the process exits. A pending body (the prompt hasn't landed
    /// yet, or a pipeline queued more) suppresses teardown so we never kill an
    /// agent before its turn actually ran.
    fn stop_oneshot_agent(&mut self, env: &JournalEnvelope) {
        let Some(job_id) = env.job_id.map(JobId).or_else(|| self.sole_live_agent()) else {
            return;
        };
        if self.state_machine.pending_count(job_id) > 0 {
            return;
        }
        // A `--once` job with a sink/terminal binding tears down via
        // `write_agent_sink` AFTER it surfaces the turn's text (on the
        // `AgentFinalResponse` envelope, which arrives just after this `Stop`).
        // Tearing down here would drop the entry's recipe before the text is
        // delivered. Defer: the AFR-driven path owns the teardown — but ONLY
        // when that envelope can actually arrive. The FinalResponseService
        // bails on a `Stop` without a `job_id` (no extraction, no AFR), so a
        // recipe-bearing job resolved via the sole-live-agent fallback would
        // otherwise wait forever (#8: a one-shot must reach a terminal state).
        if env.job_id.is_some()
            && self
                .jobs
                .get(job_id)
                .is_some_and(|e| e.sink_recipe.is_some())
        {
            return;
        }
        tracing::info!(job = job_id.0, "oneshot -c: turn complete, stopping agent");
        self.finish_oneshot(job_id);
    }

    /// Tear down a one-shot agent: SIGTERM the session and emit the authoritative
    /// `Completed` signal. Shared by [`Self::stop_oneshot_agent`] (no-AFR fallback)
    /// and [`Self::write_agent_sink`] (after surfacing the `--once` text).
    ///
    /// Best-effort kill; `stop` errors if the child already exited. The delivered
    /// turn is the authoritative completion, so `Completed` (exit 0) is emitted
    /// directly rather than depending on the PTY-exit reap of the just-SIGTERM'd
    /// agent — that reap races the engine reader's one-shot `try_wait` and can
    /// strand the entry in `Stopped`, so a detached runtime would forward only
    /// `Stopped` and the main REPL would never render `[1] done`.
    /// `oneshot_complete` is what `run_one_command`'s poll loop breaks on.
    fn finish_oneshot(&mut self, job_id: JobId) {
        let _ = self.jobs.stop(job_id);
        self.jobs.complete(job_id, 0);
        self.oneshot_complete = true;
    }

    /// The single live agent job, when there is exactly one. Used by the
    /// one-shot teardown to attribute a `Stop` hook that arrived without a
    /// `job_id` (a `-c` dispatch only ever has one agent in flight).
    fn sole_live_agent(&mut self) -> Option<JobId> {
        let agents: Vec<JobId> = self
            .jobs
            .list()
            .into_iter()
            .filter(|j| {
                matches!(j.kind, orkia_shell_types::job::JobKind::Agent { .. })
                    && matches!(
                        j.state,
                        JobState::Running | JobState::Foreground | JobState::Stopped
                    )
            })
            .map(|j| j.id)
            .collect();
        (agents.len() == 1).then(|| agents[0])
    }

    /// into the job's bound sink command. The recipe lookup is on the REPL
    /// thread (single owner); the read + `sh -c` run on a detached task so the
    /// REPL never blocks on I/O (#1). `--once` drops the binding and stops the
    /// session after this turn.
    fn write_agent_sink(&mut self, env: &JournalEnvelope) {
        let Some(id) = env.job_id else {
            tracing::warn!("agent sink: AFR had no job_id");
            return;
        };
        let job_id = JobId(id);
        let Some(recipe) = self.jobs.get(job_id).and_then(|e| e.sink_recipe.clone()) else {
            // Normal case: most agent jobs have no sink binding — every
            // completed turn lands here. Debug, not warn.
            tracing::debug!(
                job = id,
                has_entry = self.jobs.get(job_id).is_some(),
                "agent sink: no sink_recipe for job"
            );
            return;
        };
        // Extraction failure: the AFR carries no `response_path` (the
        // FinalResponseService emits a failure envelope after its retries).
        // There is no text to surface, but a `--once` binding must still reach
        // the teardown below — returning here would hang the dispatched
        // command forever (#8: a one-shot must reach a terminal state).
        match env.response_path.clone() {
            Some(path) => self.dispatch_sink_write(job_id, env, &recipe.target, path.into()),
            None => {
                tracing::warn!(
                    job = id,
                    preview = env.response_preview.as_deref(),
                    "agent sink: AFR had no response_path; skipping sink write"
                );
                // A `--once` run is usually cron (`orkia -c`), where tracing is
                // invisible — surface WHY there is no output on stderr so the
                // crond mail / log records it.
                if recipe.once {
                    eprintln!(
                        "orkia: agent --once: final-response extraction failed; no output{}",
                        env.response_preview
                            .as_deref()
                            .map(|p| format!(" ({p})"))
                            .unwrap_or_default()
                    );
                }
            }
        }
        // `--once` (incl. standalone `--once` → terminal): the binding is spent;
        // drop it and tear the session down. Done here, AFTER surfacing the text
        // (when there was any), so the response is always delivered first.
        // `stop_oneshot_agent` defers to this path whenever a recipe is present
        // (see its guard).
        if recipe.once {
            if let Some(entry) = self.jobs.get_mut(job_id) {
                entry.sink_recipe = None;
            }
            self.finish_oneshot(job_id);
        }
    }

    /// Run one bound sink delivery for a completed turn: audit it, then hand
    /// the actual I/O to a detached task so the REPL drain never blocks (#1).
    fn dispatch_sink_write(
        &mut self,
        job_id: JobId,
        env: &JournalEnvelope,
        target: &crate::job::SinkTarget,
        path: std::path::PathBuf,
    ) {
        use crate::job::SinkTarget;
        let agent = env.agent.clone().unwrap_or_default();
        let label = match target {
            SinkTarget::Command { sink_cmd, .. } => sink_cmd.clone(),
            SinkTarget::Terminal => "<terminal>".to_string(),
        };
        tracing::info!(job = job_id.0, sink = %label, path = %path.display(), "agent sink: writing");
        self.emit_audit_event(
            job_id,
            &agent,
            "agent.sink.write",
            serde_json::json!({ "agent": agent, "sink_cmd": label, "path": path }),
        );
        match target.clone() {
            SinkTarget::Command { sink_cmd, cwd, env } => {
                tokio::spawn(run_sink(sink_cmd, cwd, env, path));
            }
            SinkTarget::Terminal => {
                // the clean per-turn text as raw bytes to stdout. A `Terminal`
                // binding only exists on `--once` (`dispatch_agent_maybe_once`),
                // and `write_agent_sink` tears the session down right after this
                // call — a spawned task races the dispatching `-c` process's
                // exit and silently loses the response. The persisted file is
                // byte-capped, so the synchronous write is bounded and the
                // process is about to die anyway.
                write_terminal_sink_blocking(&path);
            }
        }
    }

    /// Convert a hook-driven `PermissionRequest` envelope into a
    /// `PendingApproval` and push it onto the shared queue. The
    /// envelope's `action`/`risk`/`description` map straight onto
    /// `ApprovalRequest`. Returns silently when the envelope has no
    /// `job_id` or when an approval is already pending for that job.
    pub(crate) fn absorb_hook_approval(&mut self, env: &JournalEnvelope) {
        let Some(id) = env.job_id else { return };
        let request = crate::approval::ApprovalRequest {
            action: env
                .action
                .clone()
                .or_else(|| env.tool.clone())
                .unwrap_or_else(|| "request".into()),
            description: env.description.clone(),
            risk: env.risk.clone(),
            files_changed: None,
            metadata: None,
        };
        if self.approvals.push_from_hook(JobId(id), request) {
            self.attention
                .blocking_approval(crate::attention::BlockingApprovalInput {
                    job_id: JobId(id),
                    agent: env.agent.clone().unwrap_or_else(|| format!("job:{id}")),
                    action: env
                        .action
                        .clone()
                        .or_else(|| env.tool.clone())
                        .unwrap_or_else(|| "request".into()),
                    risk: env.risk.clone().unwrap_or_else(|| "unknown".into()),
                });
            tracing::debug!(job = id, "hook-driven approval queued");
        }
    }

    /// Emit one envelope through the journal channel from shell code.
    /// Used by lifecycle, shell SEAL, tell, and approval producers
    /// inside the REPL. Drops silently when the journal failed to boot
    /// (the only listener-less path).
    pub(crate) fn emit_journal(&self, env: JournalEnvelope) {
        if let Some(tx) = self.journal_tx.as_ref() {
            let _ = tx.send(env);
        }
    }
}

/// Drain the journal broadcast bus into a passive source via
/// stop-hook subscriber: a dropped subscriber lag is skipped, channel close
/// ends the task. Off the REPL drain so projection-capture callbacks never
/// run on the main loop. An active source (in-process FRS) ignores the feed
/// (`ingest_streamed` defaults to a no-op).
fn spawn_streamed_afr_subscriber(
    source: std::sync::Arc<dyn orkia_shell_types::FinalResponseSource>,
    mut bus_rx: tokio::sync::broadcast::Receiver<JournalEnvelope>,
) {
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match bus_rx.recv().await {
                Ok(env) => source.ingest_streamed(&env),
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    });
}

/// Pipe the FinalResponseService's bridge channel into the hub's ingress.
/// FRS is constructed before the hub (it is one of the hub's handlers), so
/// it cannot hold `hub.sender()` directly; this relay closes the loop once
/// the hub is up. Entering via the ingress means the `AgentFinalResponse`
/// envelope is fanned out to the broadcast bus (disk tee, detached AFR
/// forwarder) and the drain — emitting into the drain directly skips the bus.
fn spawn_frs_ingress_relay(
    mut frs_rx: tokio::sync::mpsc::UnboundedReceiver<JournalEnvelope>,
    ingress: tokio::sync::mpsc::UnboundedSender<JournalEnvelope>,
) {
    tokio::spawn(async move {
        while let Some(env) = frs_rx.recv().await {
            if ingress.send(env).is_err() {
                break;
            }
        }
    });
}

/// detached runtime's forwarder, which forwards only the `AgentFinalResponse` one
/// up to the daemon. Runs on the broadcast bus, off the listener loop (#1).
fn spawn_detached_afr_forwarder(
    forwarder: std::sync::Arc<dyn orkia_shell_types::JournalEnvelopeHook>,
    mut bus_rx: tokio::sync::broadcast::Receiver<JournalEnvelope>,
) {
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match bus_rx.recv().await {
                Ok(env) => forwarder.on_envelope(&env),
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    });
}

/// turn's response text (read from `text_path`) on stdin and closing it (EOF)
/// so aggregating filters (`wc`, `sort`) flush. Detached: all I/O is off the
/// REPL thread. `cwd`/`env` were snapshot at bind time; a non-empty `env`
/// replaces the process env exactly (brush `export` parity), else we inherit so
/// `PATH` still resolves `tee`/`grep`.
/// final-response text as raw bytes to stdout (the originating terminal),
/// followed by a trailing newline. Mirrors [`run_sink`] but the "sink" is the
/// terminal, so there is no `sh -c` — just a raw stdout write. Synchronous on
/// purpose: the caller tears the one-shot session down immediately after and
/// the `-c` process exits — a spawned task would lose that race (the file is
/// byte-capped, so the blocking write is bounded).
fn write_terminal_sink_blocking(text_path: &std::path::Path) {
    use std::io::Write;
    let text = match std::fs::read(text_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(error = %e, "terminal sink: response file unreadable");
            return;
        }
    };
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(&text);
    if !text.ends_with(b"\n") {
        let _ = out.write_all(b"\n");
    }
    let _ = out.flush();
}

async fn run_sink(
    sink_cmd: String,
    cwd: std::path::PathBuf,
    env: Vec<(String, String)>,
    text_path: std::path::PathBuf,
) {
    use tokio::io::AsyncWriteExt;
    let text = match tokio::task::spawn_blocking(move || std::fs::read(&text_path)).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "agent sink: response file unreadable");
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "agent sink: read task join failed");
            return;
        }
    };
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(&sink_cmd)
        .current_dir(&cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if !env.is_empty() {
        cmd.env_clear()
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, sink = %sink_cmd, "agent sink: spawn failed");
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&text).await;
        // `stdin` drops here → EOF, so aggregating filters emit.
    }
    let _ = child.wait().await;
}

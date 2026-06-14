// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

impl Repl {
    pub async fn run(&mut self) -> Result<(), ShellError> {
        // A human is at the terminal — see the field doc on `interactive`.
        self.interactive = true;
        // Stand up the brush session before anything else: cwd, env, and
        // aliases are all owned by brush from here on. The "for_run"
        // variant honors config (load_bashrc / load_profile) and the
        // login-shell flag the binary set on us, where the lazy path
        // used by tests stays hermetic.
        self.boot_brush_for_run().await?;

        // Single ExternalPrinter sender, cloned to every async
        // producer that needs to surface real-time toasts during
        // attach / read_line. Workers that fire outside the main
        // loop (journal listener task, state-machine worker thread)
        // all push through this; the renderer's printer worker
        // serialises onto a single rustyline `ExternalPrinter`.
        let printer = self.renderer.take_external_print_sender();

        // Stand up the journal listener now that we're inside an active
        // tokio runtime. Non-fatal: a failure to bind the socket only
        // disables hook events — the shell continues to run. The
        // listener fires LiveJournalHandlers on every received
        // envelope so hook events surface during attach.
        self.boot_journal(printer.clone());

        self.render_welcome();
        self.emit_migration_notice();
        self.emit_workspace_snapshot();
        self.emit_jobs_snapshot();

        // Hand off the state-machine event stream to a worker thread
        // that prints toasts via the renderer's ExternalPrinter in
        // real time and forwards every event back to the REPL for
        // side-effect handling (journal write, PTY injection).
        self.boot_state_machine_worker(printer.clone());

        // GC stale per-job output logs. `~/.orkia/jobs/<id>/output.log`
        // accumulates on disk indefinitely; we delete anything
        // whose mtime is > 7 days old at boot. Cheap, runs once.
        gc_old_job_logs(&self.config.data_dir);

        // SIGCHLD fast-path. Without this, bg job reaping is
        // bound to the REPL's next-prompt iteration (passive reap
        // in `JobController::list` → `emit_jobs_snapshot`). With
        // SIGCHLD we wake the drain loop immediately when any
        // child exits, so `[N]+ Done` appears the instant the
        // process terminates instead of "whenever the user types
        // next". The handler is best-effort — failure to install
        // (already installed, restricted env) falls back to the
        // passive reap.
        self.boot_sigchld_waker();

        // Stand up unified event consumers. A single fanout task owns the
        // router receiver, stamps per-job RFC scope, then feeds SEAL and the
        // notify-only operator.
        if let Some(orkia_rx) = self.orkia_event_rx.take() {
            let (seal_tx, seal_rx) = tokio::sync::mpsc::unbounded_channel();
            let (operator_tx, operator_rx) = tokio::sync::mpsc::unbounded_channel();
            crate::protocol::spawn_fanout(
                orkia_rx,
                crate::protocol::FanoutConfig {
                    job_scopes: self.reasoning_scopes.clone(),
                    outputs: vec![seal_tx, operator_tx],
                },
            );
            let manager = crate::seal::SealManager::new(self.config.data_dir.clone());
            crate::seal::spawn_consumer(
                seal_rx,
                manager,
                std::sync::Arc::clone(&self.job_projects),
            );
            crate::operator::spawn(
                operator_rx,
                crate::operator::OperatorConfig {
                    data_dir: self.config.data_dir.clone(),
                    router: self.event_router.clone(),
                    journal_tx: self.journal_tx.clone(),
                },
            );
        } else {
            tracing::warn!("seal: orkia_event_rx already taken at run() — no consumer started",);
        }

        loop {
            if self.should_exit {
                break;
            }
            self.drain_job_events();
            self.drain_journal_events();
            self.drain_state_machine_events();
            self.drain_plugin_dev_reloads();
            self.poll_approvals();
            self.emit_jobs_snapshot();
            self.refresh_completion_snapshot();
            // OSC 133 `A` — Prompt Start. Lets a parent terminal
            // (iTerm2 / Ghostty / VS Code) or an outer orkia track
            // our prompt cycle. ~5 bytes; cost rounds to zero.
            emit_osc133("A");
            let ctx = self.prompt_context();
            let line = match self.renderer.read_line(&ctx) {
                Some(line) => line,
                None => break,
            };
            // OSC 133 `B` — user submitted; about to execute.
            emit_osc133("B");
            // OSC 133 `C` — command begins. (Brush doesn't separate
            // "submit" from "execute" so B and C are back-to-back
            // for us; that's fine — both markers are still useful
            // to the parent terminal for prompt-jump anchoring.)
            emit_osc133("C");
            let tick_result = self.tick(line).await;
            // OSC 133 `D[;N]` — command finished. We don't have a
            // single scalar exit code for a REPL tick (a tick can
            // run multiple things), but the convention is `0` for
            // success, `1` for any error.
            let exit_code = if tick_result.is_ok() { 0 } else { 1 };
            emit_osc133_finished(exit_code);
            if let Err(e) = tick_result {
                self.renderer
                    .publish(RenderEvent::Block(BlockContent::Error(format!("{e}"))));
            }
        }
        // Best-effort flush of the local-to-backend publisher on exit.
        // Bounded tightly: this is the quit path, a shell must exit
        // promptly, and the stream is best-effort telemetry — not durable
        // state (journal + SEAL are written per-event).
        if let Some(handle) = self.stream_handle.take() {
            orkia_stream::shutdown(handle, std::time::Duration::from_millis(800)).await;
        }
        // Terminate the process directly instead of returning to `main`.
        // Letting `main` drop the `Repl` hangs on a field's blocking
        // `Drop` (finding #4: the shell wouldn't quit on exit/quit/Ctrl-D).
        // Durable state is already persisted; this is the standard shell
        // quit path — like bash calling `exit()`. Propagates `exit N`'s
        // code (`exit_status`, 0 for bare exit/quit/Ctrl-D). The `Result`
        // return type is preserved for the error paths above (`?`).
        std::process::exit(self.exit_status);
    }

    /// Non-interactive single-command driver. Used by `orkia -c "..."`
    /// when the input requires the full classify → route → dispatch
    /// pipeline (i.e. agent commands and orkia builtins) — pure shell
    /// commands continue to go through the bare `ShellEngine` path in
    /// `bins/orkia` for parity with the legacy cron/ssh footprint.
    ///
    /// Boots the same machinery `run()` does (brush, journal,
    /// state-machine worker, SIGCHLD waker, SEAL consumer), executes
    /// one `tick`, then polls until any spawned job reaches a terminal
    /// state. Returns the last terminal exit code observed (or `0` if
    /// the tick produced no job — e.g. a pure builtin).
    ///
    /// fires `orkia -c "@faye rfc post"` with `ORKIA_SCHEDULED=1`,
    /// the SEAL records emitted along the way pick up `origin:
    /// "scheduled"` automatically via `SealManager`, and the
    /// `agent.complete` / `agent.failed` handler in `seal::consumer`
    pub async fn run_one_command(&mut self, line: String) -> Result<i32, ShellError> {
        use orkia_shell_types::JobState;

        // `oneshot_dispatch` is set ONLY when this `-c` command carried `--once`
        // generates it). A bare `@agent` via `-c`/detached runtime is persistent:
        // the poll loop below keeps the process alive while the agent session
        // idles, so `tell`/`attach` reach it. Derived from the line (not the
        // interactive `dispatch` arm) so it stays a `-c`-only concept — the
        // interactive REPL never sets this global flag and so never has a stray
        // `--once` poison a later persistent agent's `Stop`.
        self.oneshot_dispatch = line_requests_once(&line);
        self.boot_brush_for_run().await?;
        let printer = self.renderer.take_external_print_sender();
        self.boot_journal(printer.clone());
        self.boot_state_machine_worker(printer.clone());
        gc_old_job_logs(&self.config.data_dir);
        self.boot_sigchld_waker();
        if let Some(orkia_rx) = self.orkia_event_rx.take() {
            let (seal_tx, seal_rx) = tokio::sync::mpsc::unbounded_channel();
            let (operator_tx, operator_rx) = tokio::sync::mpsc::unbounded_channel();
            crate::protocol::spawn_fanout(
                orkia_rx,
                crate::protocol::FanoutConfig {
                    job_scopes: self.reasoning_scopes.clone(),
                    outputs: vec![seal_tx, operator_tx],
                },
            );
            let manager = crate::seal::SealManager::new(self.config.data_dir.clone());
            crate::seal::spawn_consumer(
                seal_rx,
                manager,
                std::sync::Arc::clone(&self.job_projects),
            );
            crate::operator::spawn(
                operator_rx,
                crate::operator::OperatorConfig {
                    data_dir: self.config.data_dir.clone(),
                    router: self.event_router.clone(),
                    journal_tx: self.journal_tx.clone(),
                },
            );
        }

        let detached_control_rx = crate::detached_control::spawn_from_env();

        // Adopt the parent REPL's `rfc cd` scope when the daemon forwarded it
        // (see `Repl::rfc_scope_env`). This must precede `tick`: the dispatch
        // it drives seals the agent's `agent.spawn` genesis with `rfc_scope`'s
        // rfc_id, which is what gives the operator (and the cross-session
        // reconciler) the RFC grounding the parent had.
        self.adopt_rfc_scope_from_env();

        // Single tick — same dispatch the interactive REPL would do.
        // Errors from the tick itself are surfaced as the function's
        // error; job-spawn failures land in Outcome::Error and render
        // through the renderer (stdout for -c, which is what we want).
        self.tick(line).await?;

        // agent this detached runtime just spawned. The relay splices the
        // agent's nested PTY ↔ this runtime's controlling terminal (which the
        // daemon captures), so `attach` reaches the live claude TUI and
        // `tell`-injected keystrokes feed the persistent session. Detached
        // runtime only (`detached_control_rx.is_some()`); the main REPL spawns
        // through the daemon and never reaches this. The relay owns its I/O on
        // dedicated threads (CLAUDE.md #1), so the poll loop below keeps
        // draining events; the handle stays bound for the loop's lifetime and
        // is torn down on the unwind paths (`process::exit` skips Drop on the
        // happy path — the OS reaps the threads alongside the dying agent).
        //
        // Persistent dispatch ONLY (`!oneshot_dispatch`). A `--once` agent's
        // contract is the clean final-response text via its Terminal sink
        // onto the same PTY would pollute that one-shot output, and a one-shot
        // is never attached/told, so it has nothing to foreground for.
        let _foreground_relay = (detached_control_rx.is_some() && !self.oneshot_dispatch)
            .then(|| self.start_foreground_relay())
            .flatten();
        // the renderer — its remaining JobUpdate / SystemInfo lines go to stderr,
        // but stderr and stdout are the SAME captured PTY here, so they would
        // interleave with the agent's live TUI. Mute only when a relay actually
        // started (persistent detached agent); a one-shot keeps its renderer so
        // builtin / sink output still reaches the originating terminal.
        if _foreground_relay.is_some() {
            self.renderer.mute();
        }

        // Wait for any spawned job(s) to finish. Mirrors the
        // interactive loop body minus the prompt: drain → poll. 100 ms
        // is the same heartbeat used elsewhere for background work
        // (`emit_jobs_snapshot`); fast enough to feel responsive,
        // slow enough to not pin the CPU.
        //
        // a jobless `-c` line — builtin success/failure/usage error —
        // exits with that code; a spawned job's terminal state then
        // overrides it below.
        let mut last_exit = self.last_outcome_status;
        loop {
            self.drain_job_events();
            self.drain_journal_events();
            self.drain_state_machine_events();
            self.drain_plugin_dev_reloads();
            self.poll_approvals();
            if let Some(rx) = &detached_control_rx {
                self.drain_detached_control(rx);
            }
            let jobs = self.jobs.list();
            let mut any_live = false;
            for j in &jobs {
                match &j.state {
                    JobState::Running | JobState::Foreground => {
                        any_live = true;
                    }
                    // Under `--once` (oneshot_dispatch): a `Stopped` agent is
                    // the teardown in `stop_oneshot_agent` — no interactive user
                    // to resume it, so it is terminal. Counting it as live would
                    // hang the loop forever when the PTY-exit reap races with the
                    // SIGCHLD waker and the entry lingers in `Stopped` instead of
                    // flipping to `Done`. For a persistent dispatch (no `--once`)
                    // a `Stopped` agent IS still live — it can be resumed.
                    JobState::Stopped => {
                        if !self.oneshot_dispatch {
                            any_live = true;
                        }
                    }
                    JobState::Done { exit_code } => {
                        last_exit = *exit_code;
                    }
                    JobState::Failed { .. } => {
                        last_exit = 1;
                    }
                }
            }
            // A one-shot agent that delivered its turn (`stop_oneshot_agent`)
            // is terminal even if its job entry has not yet reaped to `Done`.
            if !any_live || self.oneshot_complete {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // One last drain so any terminal events (agent.complete →
        // pending parking, scheduled_failure journal append) land
        // before we exit. For a detached runtime this is where the
        // `Completed` emitted by `stop_oneshot_agent` is handed to the
        // forwarding observer.
        self.drain_job_events();
        self.drain_journal_events();
        // A detached runtime forwards `JobEvent`s to the daemon on a
        // background thread (the socket I/O must not block the REPL drain).
        // The terminal `Completed` was only just queued by the final drain
        // above, so block briefly until the forwarder has actually sent it —
        // otherwise `process::exit` below tears the runtime down mid-send and
        // the main REPL never renders `[1] done`. No observer (main REPL) or
        // nothing buffered ⇒ this returns immediately.
        if let Some(observer) = &self.job_event_observer {
            observer.flush_pending(std::time::Duration::from_secs(2));
        }
        // Best-effort flush of the local-to-backend publisher, same
        // bound as the interactive quit path — best-effort telemetry,
        // not durable state (journal + SEAL are written per-event).
        if let Some(handle) = self.stream_handle.take() {
            orkia_stream::shutdown(handle, std::time::Duration::from_millis(800)).await;
        }
        // Terminate the process directly instead of returning to `main`.
        // A one-shot `-c` has delivered its single turn (or its builtin),
        // so this is its quit path. Returning would let `main` drop the
        // `Repl`, whose `JournalStore` `Drop` blocks forever joining its
        // writer thread (finding #4 — the same blocking `Drop` the
        // interactive `run()` loop above sidesteps with `process::exit`).
        // For the detached inner runtime this is load-bearing: the daemon
        // only sees the job reach a terminal state (so `orkia wait` returns
        // `done`) once this PID actually exits. Durable state is already
        // persisted per-event; the `Result` return type is kept for the
        // error paths above (`?`).
        std::process::exit(last_exit);
    }

    /// Re-enter the `rfc cd` scope the parent REPL forwarded through
    /// `ORKIA_RFC_PROJECT` / `ORKIA_RFC_ID` (set by [`Repl::rfc_scope_env`] on
    /// the daemon spawn path). Only the daemon sets these — a plain
    /// `orkia -c` from a user shell leaves them unset, so this is a no-op
    /// there. We trust the parent's validation (it confirmed the RFC on disk
    /// via `rfc cd`) and skip if a scope is somehow already active.
    fn adopt_rfc_scope_from_env(&mut self) {
        if self.rfc_scope.is_some() {
            return;
        }
        let (Ok(project), Ok(id)) = (
            std::env::var("ORKIA_RFC_PROJECT"),
            std::env::var("ORKIA_RFC_ID"),
        ) else {
            return;
        };
        if project.is_empty() || id.is_empty() {
            return;
        }
        self.rfc_scope = Some(super::RfcScopeState {
            project,
            rfc_id: orkia_rfc_core::RfcId::new(id),
        });
    }

    fn drain_detached_control(
        &mut self,
        rx: &std::sync::mpsc::Receiver<crate::detached_control::ControlCommand>,
    ) {
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                crate::detached_control::ControlCommand::List { respond } => {
                    let stages = self
                        .jobs
                        .list()
                        .into_iter()
                        .map(|job| {
                            let target = match &job.kind {
                                JobKind::Agent { agent_name, .. } => {
                                    format!("@{agent_name}")
                                }
                                JobKind::Shell { cmd } => cmd.clone(),
                                JobKind::ForgeApp { app_name } => app_name.clone(),
                            };
                            crate::detached_control::StageInfo {
                                id: job.id.0,
                                target,
                                state: job.state.to_string(),
                                pid: job.pid,
                                runtime_secs: job.runtime.as_secs(),
                                // A native session has no PTY to splice — never
                                // attachable, whatever its state.
                                attachable: matches!(
                                    job.state,
                                    JobState::Running | JobState::Foreground | JobState::Stopped
                                ) && self.jobs.native_inbound(job.id).is_none(),
                                lost_reason: None,
                                exit_code: match job.state {
                                    JobState::Done { exit_code } => Some(exit_code),
                                    _ => None,
                                },
                            }
                        })
                        .collect();
                    let _ = respond.send(crate::detached_control::ControlResponse::List { stages });
                }
                crate::detached_control::ControlCommand::Tell {
                    target,
                    message,
                    respond,
                } => {
                    let target = target.trim_start_matches('@');
                    let outcome = self.handle_tell(target, &message);
                    let _ = respond.send(control_response_from_outcome(outcome));
                }
                crate::detached_control::ControlCommand::Kill { target, respond } => {
                    let target = target.trim_start_matches('@');
                    let outcome = self.handle_kill(target, None);
                    let _ = respond.send(control_response_from_outcome(outcome));
                }
                crate::detached_control::ControlCommand::Attach {
                    target,
                    winsize,
                    stream,
                } => {
                    self.spawn_detached_stage_attach(
                        target.trim_start_matches('@'),
                        winsize,
                        stream,
                    );
                }
            }
        }
    }

    fn spawn_detached_stage_attach(
        &mut self,
        target: &str,
        winsize: Option<(u16, u16)>,
        stream: std::os::unix::net::UnixStream,
    ) {
        let jobs = self.jobs.list();
        let Some(id) = crate::builtin_resolve::resolve_job_target(target, &jobs) else {
            crate::detached_control::write_attach_error(
                stream,
                format!("attach: no such job: {target}"),
            );
            return;
        };
        if self.jobs.native_inbound(id).is_some() {
            crate::detached_control::write_attach_error(
                stream,
                format!(
                    "[{}] is a native session — it has no terminal. \
                     Use `tell {}`, `journal --job {}`, or the final response",
                    id.0, id.0, id.0
                ),
            );
            return;
        }
        let Some(entry) = self.jobs.get(id) else {
            crate::detached_control::write_attach_error(stream, format!("job {id} not found"));
            return;
        };
        // Resize to the attaching terminal BEFORE snapshotting: the agent
        // PTY was sized from the runtime's (headless fallback 120×42), so
        // the catch-up paint would otherwise render at the wrong geometry.
        if let Some((cols, rows)) = winsize {
            let _ = entry.engine.resize(cols as usize, rows as usize);
        }
        let history = entry.engine.history_snapshot();
        let snapshot = entry.engine.render_visible_snapshot();
        let rx = entry.engine.subscribe_output();
        let writer = entry.engine.writer();
        let name = format!("orkia-detached-stage-attach-{}", id.0);
        if std::thread::Builder::new()
            .name(name)
            .spawn(move || {
                crate::detached_control::pump_stage_attach(stream, history, snapshot, rx, writer);
            })
            .is_err()
        {
            // The stream has moved into the attempted closure, so there is no
            // remaining channel to report failure without complicating ownership.
        }
    }

    /// relay for the single agent a detached runtime just spawned. Returns
    /// `None` when there is no single live agent to foreground (a builtin `-c`,
    /// an already-exited agent, or a multi-job runtime — a pipeline keeps its
    /// own per-stage attach path, and splicing one stage to the shared terminal
    /// would interleave output) or when the relay could not enter raw mode.
    fn start_foreground_relay(&mut self) -> Option<crate::job::foreground_relay::ForegroundRelay> {
        let jobs = self.jobs.list();
        let live: Vec<&orkia_shell_types::JobInfo> = jobs
            .iter()
            .filter(|j| {
                matches!(j.state, JobState::Running | JobState::Foreground)
                    && matches!(j.kind, JobKind::Agent { .. })
            })
            .collect();
        let [only] = live.as_slice() else {
            return None;
        };
        let engine = &self.jobs.get(only.id)?.engine;
        match crate::job::foreground_relay::ForegroundRelay::start(engine) {
            Ok(relay) => Some(relay),
            Err(e) => {
                tracing::warn!("foreground relay: failed to start: {e}");
                None
            }
        }
    }

    pub async fn tick(&mut self, line: String) -> Result<(), ShellError> {
        let trimmed = line.trim().to_string();

        // `exit` / `quit` at the REPL top level quit the shell. Intercept
        // here, before classification/brush routing, so they reliably set
        // the loop's exit flag — finding #4: routed through the normal
        // path they never terminated (and `exit` could even hang).
        // `exit N` propagates the code N (bash-style); `run` exits with it.
        if is_quit_command(&trimmed) {
            self.exit_status = quit_exit_code(&trimmed, self.last_outcome_status);
            self.should_exit = true;
            return Ok(());
        }

        let (trimmed, background) = parse_background(trimmed);

        // capture stage below — `!echo hi | @faye` is a brush byte
        // pipeline, never a shell-to-agent pipe or an agent spawn. All
        // leading bangs are stripped (historic quirk, kept and pinned).
        if trimmed.starts_with('!') {
            let decision = Decision::Shell(trimmed.trim_start_matches('!').trim().to_string());
            self.record_history(&trimmed, &decision, &Mode::Shell);
            let outcome = self.dispatch(&decision, background, &trimmed).await;
            self.render_outcome(&outcome);
            self.drain_job_events();
            return Ok(());
        }

        // before mode resolution. The split is quote-aware and only
        // fires when an `@` follows the (rightmost) `|`. On parse
        // success we bypass the normal classifier — a shell-to-agent
        // pipe is its own dispatch path regardless of whether the
        // prefix would otherwise be classified as Shell or Contextual.
        //
        // Multi-agent pipelines (`@a | @b`, `<shell> | @a | @b`) are
        // intentionally NOT caught here — they fall through to the
        // legacy `parse_agent_or_pipeline` path which produces
        // `Decision::Pipeline`. That dispatcher routes to the Team
        // coordinator if wired, else returns a clear Team-required
        match crate::shell_agent_pipe::parse_shell_to_agent(&trimmed) {
            Ok(parse) => {
                let decision = Decision::ShellToAgent {
                    shell: parse.shell,
                    agent: parse.agent,
                    body: parse.body,
                };
                self.record_history(&trimmed, &decision, &Mode::Contextual);
                let outcome = self.dispatch(&decision, background, &trimmed).await;
                self.render_outcome(&outcome);
                self.drain_job_events();
                return Ok(());
            }
            Err(crate::shell_agent_pipe::ParseError::NotAShellAgentPipe) => {
                // Fall through to regular classification.
            }
            Err(crate::shell_agent_pipe::ParseError::MultiAgentPipeline) => {
                // `@a | @b` is handled by the legacy `parse_pipeline`
                // (starts with `@` → Mode::Agent → `parse_agent_or_pipeline`).
                // `<shell> | @a | @b` (mixed) does not — the first
                // token isn't `@`, so the classifier picks Shell and
                // brush fails. Catch it explicitly: synthesize the
                // agent stages, drop the shell prefix (Solo can't run
                // it; the coordinator wraps it on Team), and route
                // through `Decision::Pipeline`. The dispatcher emits
                // a Team-required error when no coordinator is wired.
                if !trimmed.starts_with('@')
                    && let Some(stages) = synthesize_agent_chain_stages(&trimmed)
                {
                    let decision = Decision::Pipeline(stages);
                    self.record_history(&trimmed, &decision, &Mode::Contextual);
                    let outcome = self.dispatch(&decision, background, &trimmed).await;
                    self.render_outcome(&outcome);
                    self.drain_job_events();
                    return Ok(());
                }
                // Pure `@a | @b` falls through to the legacy parser.
            }
            Err(e @ crate::shell_agent_pipe::ParseError::MissingAgentName) => {
                self.render_outcome(&Outcome::Error(format!("pipeline: {e}")));
                return Ok(());
            }
        }

        // the registry-backed streaming engine. Returns `None` for POSIX pipes
        // (e.g. `ls | grep`) and unknown commands, which fall through to the
        // legacy classification below — the ByteStream pipe stays the default.
        if let Some(outcome) = self.dispatch_attention_effectful(&trimmed) {
            let decision = Decision::Builtin {
                name: "attention".into(),
                args: tokenize_args(trimmed.trim()).into_iter().skip(1).collect(),
            };
            self.record_history(&trimmed, &decision, &Mode::Builtin);
            self.render_outcome(&outcome);
            self.drain_job_events();
            return Ok(());
        }

        if let Some(plan) = crate::exec::try_parse_exec(&trimmed, &self.registry) {
            let decision = Decision::Exec(plan);
            self.record_history(&trimmed, &decision, &Mode::Builtin);
            let outcome = self.dispatch(&decision, background, &trimmed).await;
            self.render_outcome(&outcome);
            self.drain_job_events();
            return Ok(());
        }

        // the left of a pipe. Into a *typed* registry command (`where`) it stays
        // a type error — the agent emits text, not the structured Table the
        // command expects. Into an *external* shell command (`tee`, `grep`) it
        // binds a sink: the agent's per-turn response is piped into it. Agent-to-
        // agent pipes (`@a | @b`) are not caught here — they route to the Team path.
        match crate::exec::classify_agent_on_left(&trimmed, &self.registry) {
            crate::exec::AgentLeft::TypeMismatch(error) => {
                // pipeline never ran. `ExecError::exit_code` owns the split.
                let outcome = if error.exit_code() == 2 {
                    Outcome::UsageError(error.to_string())
                } else {
                    Outcome::Error(error.to_string())
                };
                self.render_outcome(&outcome);
                self.drain_job_events();
                return Ok(());
            }
            crate::exec::AgentLeft::Sink {
                agent,
                body,
                once,
                sink_cmd,
            } => {
                let decision = Decision::AgentToSink {
                    agent,
                    body,
                    once,
                    sink_cmd,
                };
                self.record_history(&trimmed, &decision, &Mode::Contextual);
                let outcome = self.dispatch(&decision, background, &trimmed).await;
                self.render_outcome(&outcome);
                self.drain_job_events();
                return Ok(());
            }
            crate::exec::AgentLeft::NotAgentOnLeft => {}
        }

        let mode = resolve_mode(&trimmed);
        let mode_for_history = mode.clone();

        let decision = match mode {
            Mode::Shell => Decision::Shell(trimmed.trim_start_matches('!').trim().to_string()),
            // builtin argument parser — bare collidable heads reroute to
            // brush as plain POSIX, native heads refuse loudly.
            Mode::Builtin => match crate::operator_routing::route_builtin_operator(&trimmed) {
                Some(crate::operator_routing::OperatorRoute::Brush) => {
                    Decision::Shell(trimmed.clone())
                }
                Some(crate::operator_routing::OperatorRoute::Refuse { head }) => {
                    self.render_outcome(&Outcome::UsageError(
                        crate::operator_routing::refusal_message(&head),
                    ));
                    return Ok(());
                }
                // argument shape falls outside the builtin grammar yields
                // the whole line to brush (`ps aux`, `route -n get default`).
                None if crate::shape_routing::bare_shape_yields_to_brush(&trimmed) => {
                    Decision::Shell(trimmed.clone())
                }
                None => self.parse_builtin(&trimmed)?,
            },
            Mode::Agent(name) => self.parse_agent_or_pipeline(&trimmed, name)?,
            Mode::Contextual => {
                if trimmed.is_empty() {
                    Decision::NoOp(NoOpReason::Empty)
                } else {
                    // Kernel classification does a blocking socket round-trip;
                    // run it off the async reactor so a slow kernel can't freeze
                    // the REPL loop (BUG-039).
                    let classifier = std::sync::Arc::clone(&self.classifier);
                    let line = trimmed.clone();
                    let guess = tokio::task::spawn_blocking(move || classifier.classify(&line))
                        .await
                        .unwrap_or(IntentGuess::Command);
                    match guess {
                        IntentGuess::Command => Decision::Shell(trimmed.clone()),
                        IntentGuess::Agent => {
                            let agent_name = self.router.route(&trimmed, &self.agents).map(|r| {
                                self.renderer.publish(RenderEvent::RoutingInfo {
                                    agent: r.agent_name.clone(),
                                    confidence: r.confidence,
                                    reason: format!("{:?}", r.reason),
                                });
                                r.agent_name
                            });
                            // Natural-language routing is not the explicit
                            // `@agent … --once` form, so it is always persistent.
                            Decision::Agent {
                                name: agent_name,
                                body: trimmed.clone(),
                                once: false,
                            }
                        }
                    }
                }
            }
        };

        if matches!(decision, Decision::NoOp(_)) {
            return Ok(());
        }

        // `decision` / `outcome` records are no longer sealed —
        // they were REPL-wide noise that didn't fit into the
        // scoped (job/project) chain model. Action history still
        // captures them via `record_history` + the journal.
        self.record_history(&trimmed, &decision, &mode_for_history);
        let outcome = self.dispatch(&decision, background, &trimmed).await;
        self.render_outcome(&outcome);
        self.drain_job_events();
        Ok(())
    }

    pub(crate) async fn dispatch(
        &mut self,
        decision: &Decision,
        background: bool,
        raw_line: &str,
    ) -> Outcome {
        match decision {
            Decision::Shell(cmd) => self.dispatch_shell(cmd, background).await,
            Decision::Builtin { name, args } => self.dispatch_named(name, args).await,
            Decision::Exec(plan) => self.dispatch_exec(plan).await,
            // REPL exit). `raw_line` is the verbatim line the detached runtime
            // re-parses — passing the raw line (not a reconstruction of `@name body`)
            // avoids re-stringifying a body that itself contains `| @other`, which
            // the classifier already decided was NOT a pipeline.
            Decision::Agent { name, body, once } => {
                let outcome = self
                    .dispatch_agent_maybe_once(name.as_deref(), body, *once, Some(raw_line))
                    .await;
                // Foreground `@agent` (interactive, no `&`) lands the user IN
                // the session: a fresh daemon spawn auto-attaches.
                self.auto_attach_foreground_spawn(outcome, background).await
            }
            Decision::Pipeline(stages) => self.dispatch_pipeline(stages).await,
            Decision::ShellToAgent { shell, agent, body } => {
                self.dispatch_shell_to_agent(shell, agent, body, Some(raw_line))
                    .await
            }
            // verbatim line — the daemon-owned runtime re-parses this same
            // AgentToSink shape (spawner None there, the recursion guard) and
            // binds the sink recipe in-process, next to its Stop-hook/final-
            // response wiring. The whole `@agent … | cmd` construct (agent AND
            // per-turn sink) survives REPL exit. Always a fresh daemon session:
            // routing through `dispatch_agent`'s live-session reuse would
            // `tell` the existing job and the recipe would never bind.
            Decision::AgentToSink {
                agent,
                body,
                once,
                sink_cmd,
            } => match self.spawn_detached_for_line(raw_line, agent) {
                Some(outcome) => outcome,
                None => {
                    self.dispatch_agent_to_sink(agent, body, *once, sink_cmd)
                        .await
                }
            },
            Decision::NoOp(_) => Outcome::Noop,
        }
    }

    /// the optional first body (same path as `@agent body` / `tell`), and attach
    /// a sink recipe so each completed turn's response is piped into `sink_cmd`.
    /// The shell engine's cwd + exported env are snapshot now (bind time) so the
    /// sink runs where the user expects.
    ///
    /// This is the IN-PROCESS half only: the main REPL flips the whole line to
    /// the daemon in `dispatch` (M2 recipe-carry) and never reaches here; the
    /// detached runtime (no spawner) lands here and binds the recipe locally.
    pub(crate) async fn dispatch_agent_to_sink(
        &mut self,
        agent: &str,
        body: &str,
        once: bool,
        sink_cmd: &str,
    ) -> Outcome {
        let outcome = self.dispatch_agent(Some(agent), body, None).await;
        let (env, cwd) = match self.ensure_brush().await {
            Ok(_) => match self.brush.clone() {
                Some(brush_arc) => {
                    let brush = brush_arc.lock().await;
                    (brush.exported_env(), brush.cwd().to_path_buf())
                }
                None => (Vec::new(), std::env::current_dir().unwrap_or_default()),
            },
            Err(_) => (Vec::new(), std::env::current_dir().unwrap_or_default()),
        };
        let target = crate::job::SinkTarget::Command {
            sink_cmd: sink_cmd.to_string(),
            cwd,
            env,
        };
        self.bind_sink_recipe(agent, target, once, sink_cmd);
        outcome
    }

    /// Plain `@agent [body] [--once]` dispatch. Persistent by default; with
    /// turn's clean final-response text is printed to stdout and the session is
    /// killed after the first `Stop`. The terminal binding only attaches to a
    /// LOCAL job entry — a daemon-owned (detached) dispatch binds its `Terminal`
    /// recipe inside the runtime, which re-parses the same `--once` line.
    pub(crate) async fn dispatch_agent_maybe_once(
        &mut self,
        name: Option<&str>,
        body: &str,
        once: bool,
        command_line: Option<&str>,
    ) -> Outcome {
        // A standalone `--once` is a BLOCKING one-shot: the dispatching
        // process owns the session, prints the final response to ITS stdout,
        // and tears down after the first Stop. The daemon flip would
        // detach the turn from the invoking terminal — the Terminal sink
        // would re-bind inside the runtime and print into the wrapper PTY,
        // so cron / `orkia -c` callers would see nothing. One-shots don't
        // outlive their invoker by design; withhold the raw line so the
        // detached-spawner gate never fires and the dispatch stays local.
        let command_line = if once { None } else { command_line };
        let outcome = self.dispatch_agent(name, body, command_line).await;
        if once && let Some(agent) = name {
            self.bind_sink_recipe(agent, crate::job::SinkTarget::Terminal, true, "<terminal>");
        }
        outcome
    }

    /// Attach a `SinkRecipe` to agent `agent`'s live LOCAL job entry and emit the
    /// `agent.sink.bind` SEAL event. No-op (beyond a missing-entry trace) when the
    /// agent has no local entry — e.g. a detached dispatch whose entry lives in
    /// the daemon runtime. `label` is the human-readable sink for the audit event.
    fn bind_sink_recipe(
        &mut self,
        agent: &str,
        target: crate::job::SinkTarget,
        once: bool,
        label: &str,
    ) {
        let Some(job_id) = self.jobs.find_live_agent_by_name(agent) else {
            return;
        };
        if let Some(entry) = self.jobs.get_mut(job_id) {
            entry.sink_recipe = Some(crate::job::SinkRecipe { target, once });
        }
        self.emit_audit_event(
            job_id,
            agent,
            "agent.sink.bind",
            serde_json::json!({ "agent": agent, "sink_cmd": label, "once": once }),
        );
    }

    pub(crate) fn render_outcome(&mut self, outcome: &Outcome) {
        // so this is the single `$?` tracking point. Shell lines keep
        // their raw i32 (brush already owns that value); everything else
        // maps through `Outcome::exit_code`.
        self.last_outcome_status = match outcome {
            Outcome::ShellComplete { exit_code, .. } => *exit_code,
            other => i32::from(other.exit_code()),
        };
        match outcome {
            Outcome::ShellComplete { output, exit_code } if !output.is_empty() => {
                self.emit_block(BlockContent::Text(output.clone()));
                self.renderer.note_exit(*exit_code);
            }
            Outcome::ShellComplete { exit_code, .. } => {
                self.renderer.note_exit(*exit_code);
            }
            Outcome::BuiltinOutput { blocks } => {
                for block in blocks {
                    self.emit_block(block.clone());
                }
            }
            Outcome::AgentStarted { agent, job_id } => {
                self.emit_block(BlockContent::SystemInfo(format!(
                    "▸ job {agent}:{job_id} spawned as background"
                )));
            }
            Outcome::JobSpawned {
                job_id,
                foreground,
                owner,
            } => {
                if !foreground {
                    // `%N` local (dies with the REPL) vs `[N]` daemon (survives) —
                    // the prefix is the survival contract, and round-trips through
                    // `parse_job_target` so the user can retype it.
                    let id = orkia_shell_types::render_job_id(*owner, job_id.0, None);
                    self.emit_block(BlockContent::SystemInfo(format!(
                        "{id} spawned as background"
                    )));
                }
            }
            Outcome::PipelineStarted { stages } => {
                self.emit_block(BlockContent::SystemInfo(format!(
                    "▸ pipeline {} stages",
                    stages.len()
                )));
            }
            Outcome::Error(e) | Outcome::UsageError(e) => {
                self.emit_block(BlockContent::Error(e.clone()));
            }
            Outcome::Noop => {}
        }
    }
}

/// True when a `-c` command line carries the standalone `--once` token
/// detached-runtime path. A whitespace-token scan is sufficient — `--once` is
/// defined as a standalone token (it never appears inside an agent body that
/// matters here), matching `exec::parse::split_agent_stage`.
fn line_requests_once(line: &str) -> bool {
    line.split_whitespace().any(|t| t == "--once")
}

fn control_response_from_outcome(outcome: Outcome) -> crate::detached_control::ControlResponse {
    match outcome {
        Outcome::Error(message) | Outcome::UsageError(message) => {
            crate::detached_control::ControlResponse::Error { message }
        }
        _ => crate::detached_control::ControlResponse::Ok,
    }
}

#[cfg(test)]
mod oneshot_line_tests {
    use super::line_requests_once;

    #[test]
    fn detects_standalone_once_token() {
        assert!(line_requests_once("@faye review the auth module --once"));
        assert!(line_requests_once("@faye rfc post --once"));
        // cron-generated form (sink) still one-shot.
        assert!(line_requests_once("@faye list --once | tee f"));
    }

    #[test]
    fn bare_dispatch_is_persistent() {
        assert!(!line_requests_once("@faye review the auth module"));
        assert!(!line_requests_once("@faye"));
        // `--once` is a standalone token; a substring must not trip it.
        assert!(!line_requests_once("@faye summarize the --oncely report"));
    }
}

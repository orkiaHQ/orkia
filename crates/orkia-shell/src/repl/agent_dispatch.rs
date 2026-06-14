// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

impl Repl {
    /// Build the spawn-time context (system prompt + memory + project
    /// rfcs/issues, MCP tools) for `name`. Returns the agent context,
    /// env vars, and the `[hooks] provider` from agent.toml when set.
    /// `(None, env, None)` for legacy/inline agents — they still spawn
    /// without injection or hooks.
    pub(crate) async fn build_agent_context(
        &self,
        name: Option<&str>,
    ) -> (
        Option<crate::agent_context::AgentContext>,
        Vec<(String, String)>,
        Option<String>,
    ) {
        let mut env = self.shell_env_for_agents().await;
        let Some(name) = name else {
            return (None, env, None);
        };
        let Some(def) = crate::agent_dir::load_definition_by_name(&self.config.data_dir, name)
        else {
            return (None, env, None);
        };
        let hooks_provider = def.hooks_provider.clone();
        let scope_filter = crate::agent_context::ScopeFilterContext {
            has_team_membership: self.team_cache.has_any_team_sync(),
            is_authenticated: self.auth_provider.is_some(),
            workspace_default: self.config.default_scope,
        };
        let mut context = crate::agent_context::AgentContext::load_with_filter(
            &def,
            &self.workspace,
            &scope_filter,
        );
        // Inject the preference block into the assembled context the agent
        // receives. Synchronous, cache-only — never a network call. Exact
        // pass-through when reasoning is inert (gate closed / not booted).
        if let Some(intel) = self.intelligence.as_ref() {
            context.assembled = intel.enrich_active(&context.assembled, None);
            context.knowledge_mcp_bridge = intel.is_active();
        }
        env.push((
            "ORKIA_AGENT_MEMORY".into(),
            def.memory_path().to_string_lossy().into_owned(),
        ));
        (Some(context), env, hooks_provider)
    }

    /// Queue `body` for delivery to an already-running agent job.
    /// The state machine handles when to actually write it: the
    /// detector waits for the agent to be idle ≥ 1.5 s before
    /// firing `emit_injection`. Multiple consecutive `@agent body`
    /// calls just append to the FIFO; bodies drain in order, one
    /// per idle window — never interrupting an agent mid-render.
    pub(crate) fn deliver_to_existing_agent(
        &mut self,
        job_id: JobId,
        agent_name: &str,
        body: &str,
    ) -> Outcome {
        let body = body.trim();
        if body.is_empty() {
            return Outcome::BuiltinOutput {
                blocks: vec![BlockContent::SystemInfo(format!(
                    "tell: empty body, nothing to send to [{}]",
                    job_id.0
                ))],
            };
        }
        if !self.state_machine.append_body(job_id, body.to_string()) {
            return Outcome::Error(format!(
                "deliver: job {} is not tracked by state machine",
                job_id.0
            ));
        }
        let depth = self.state_machine.pending_count(job_id);
        self.attention
            .queued_input(crate::attention::QueuedInputInput {
                job_id,
                agent: agent_name.to_string(),
                depth,
                body: body.to_string(),
            });
        // Journal at queue-time so external observers see intent
        // even if the body is still waiting in the FIFO. The
        // `emit_injection: DONE` log will mark actual delivery.
        let mut env = JournalEnvelope::now(EventType::Tell);
        env.job_id = Some(job_id.0);
        env.agent = Some(agent_name.to_string());
        env.source = Some("orkia".into());
        env.message = Some(body.to_string());
        self.emit_journal(env);
        let queue_hint = if depth > 1 {
            format!(" \x1b[90m(queued, {depth} ahead)\x1b[0m")
        } else {
            String::new()
        };
        Outcome::BuiltinOutput {
            blocks: vec![BlockContent::SystemInfo(format!(
                "  \x1b[36m▸\x1b[0m queued for [{}] \x1b[90m({})\x1b[0m{queue_hint}",
                job_id.0, agent_name
            ))],
        }
    }

    /// Dispatch `body` to agent `name`. `command_line` is the verbatim REPL line
    /// for the daemon flip: when `Some` AND a `detached_spawner` is
    /// installed (the main REPL), the agent spawns in the `pty_daemon` so it
    /// survives REPL exit, with the runtime re-parsing `command_line`. `None`
    /// (operator-ask synthesis, sink-bound agents) keeps the agent in-process —
    /// those paths need the REPL-local JobController entry (synchronous final-
    /// response capture / sink-recipe binding) the daemon path doesn't provide.
    /// daemon-owned agent named `name` is still live (non-terminal in the
    /// roster), deliver `body` to it via the daemon `tell` instead of spawning a
    /// duplicate. `None` when there is no daemon bridge, no live match, or `name`
    /// is absent — the caller then proceeds to spawn. A bare `@faye` (empty body)
    /// with faye already live reports the existing job rather than re-spawning.
    fn deliver_to_live_daemon_agent(&self, name: Option<&str>, body: &str) -> Option<Outcome> {
        let name = name?;
        let bridge = self.daemon_jobs.as_ref()?;
        let view = bridge
            .list()
            .into_iter()
            .rev()
            .find(|v| v.agent == name && daemon_view_is_live(v))?;
        let body = body.trim();
        if body.is_empty() {
            return Some(Outcome::BuiltinOutput {
                blocks: vec![BlockContent::SystemInfo(format!(
                    "[{}] {} already running — use `tell {}` or `attach {}`",
                    view.id, name, view.id, view.id
                ))],
            });
        }
        Some(self.tell_daemon_job(view.id, body))
    }

    /// Dispatch `body` to agent `name`. `command_line` is the verbatim REPL line
    /// for the daemon flip: when `Some` AND a `detached_spawner` is
    /// installed (the main REPL), the agent spawns in the `pty_daemon` so it
    /// survives REPL exit, with the runtime re-parsing `command_line`. `None`
    /// (operator-ask synthesis, the detached runtime's in-process sink half —
    /// see `dispatch_agent_to_sink`) keeps the agent in-process — those paths
    /// need the REPL-local JobController entry (synchronous final-response
    /// capture / sink-recipe binding) the daemon path doesn't provide.
    pub(crate) async fn dispatch_agent(
        &mut self,
        name: Option<&str>,
        body: &str,
        command_line: Option<&str>,
    ) -> Outcome {
        let agent_name = name.unwrap_or("unrouted");

        // `@agent execute rfc <slug>` shortcut: route through the rfc delegate flow.
        if let Some(real) = name
            && let Some(slug) = body.trim().strip_prefix("execute rfc ")
        {
            let slug = slug.trim();
            if !slug.is_empty() {
                let project = match self.resolve_rfc_project(None) {
                    Ok(p) => p,
                    Err(o) => return o,
                };
                return self.dispatch_rfc_delegate(slug, &project, real).await;
            }
        }

        // Reuse an already-running agent instead of spawning a fresh
        // one. The user's intent `@faye <body>` (or a heuristic-
        // routed question) when faye is already alive should *send*
        // the body to that running session, the same way `tell <job>`
        // would. Spawning a new job here was wasting model context
        // and confusing the user: `[3] spawned: agent:faye` when job
        // 1 is still waiting.
        if let Some(real) = name
            && let Some(existing) = self.jobs.find_live_agent_by_name(real)
        {
            return self.deliver_to_existing_agent(existing, real, body);
        }

        // flip a running `@faye` lives in the daemon roster, not the local
        // JobController, so `find_live_agent_by_name` misses it — without this a
        // second `@faye` would spawn a duplicate daemon session instead of
        // telling the live one. Main REPL only (a detached runtime has no bridge).
        if command_line.is_some()
            && let Some(outcome) = self.deliver_to_live_daemon_agent(name, body)
        {
            return outcome;
        }

        // `[runtime] type = "native"`: no vendor CLI, no PTY — route to
        // the Orkia-owned loop (see `native_dispatch.rs`). Forked before
        // command resolution because native agents have no command.
        if let Some(real) = name
            && self.config.native_agents.contains(real)
        {
            return self.dispatch_native(real, body, command_line).await;
        }

        let (cmd, args) = match name.and_then(|n| self.config.resolve_agent(n)) {
            Some((cmd, args)) => (cmd.to_string(), args.to_vec()),
            None => {
                return Outcome::AgentStarted {
                    agent: agent_name.into(),
                    job_id: self.config.agent_unresolved_reason(agent_name),
                };
            }
        };

        let (agent_context, extra_env, hooks_provider) = self.build_agent_context(name).await;

        // Trust gate: an agent refuses to run in a directory the user
        // hasn't approved. Ask once via a Yes/No select modal; on Yes,
        // record it + pre-trust the provider config, then spawn.
        if let Some(dir) = self.agent_cwd() {
            let provider = orkia_shell_types::ProviderId::derive(hooks_provider.as_deref(), &cmd);
            if !self.dir_is_trusted(&dir, provider) {
                let detail = format!(
                    "{agent_name} will read, edit, and run files in {}",
                    dir.display()
                );
                let choice = self.renderer.select_prompt(
                    "Trust this directory?",
                    &detail,
                    &["Yes, trust this folder", "No, cancel"],
                    0,
                );
                if choice != Some(0) {
                    return Outcome::BuiltinOutput {
                        blocks: vec![BlockContent::Notice {
                            style: CellStyle::Warn,
                            text: format!(
                                "  trust declined — {agent_name} not started in {}",
                                dir.display(),
                            ),
                        }],
                    };
                }
                if let Err(e) = self.trust_registry.trust(&dir) {
                    return Outcome::Error(format!("trust: registry write failed: {e}"));
                }
                if let Some(home) = trust_home() {
                    match crate::trust::provider_for(provider, home).pretrust(&dir) {
                        Ok(crate::trust::PreTrust::Ensured) => tracing::info!(
                            provider = provider.as_str(), dir = %dir.display(),
                            "trust: pre-trusted provider config",
                        ),
                        Ok(crate::trust::PreTrust::Unsupported) => tracing::info!(
                            provider = provider.as_str(),
                            "trust: no provider config integration; relying on auto-answer",
                        ),
                        Err(e) => tracing::warn!(
                            provider = provider.as_str(),
                            "trust: pretrust failed ({e}); relying on auto-answer",
                        ),
                    }
                }
            }
        }

        // REPL exit. The detached runtime re-parses `line` through the identical
        // classifier → dispatch, re-deriving agent context / cage / hooks from
        // its own config and delivering the first body via its OWN detector — so
        // we forward only the raw line + cwd + name, not the in-process job
        // config. Trust was just resolved REPL-side (the human is here); the
        // daemon pre-trusts `working_dir` so the runtime won't re-prompt. The
        // REPL learns of the running job via the push stream and the
        // daemon roster. `record_reasoning_scope` / `emit_public_job_on_spawn`
        // are NOT called here — they need the REPL-local JobController entry a
        // daemon job lacks; the runtime owns reasoning attribution, and public-
        // job events are forwarded up from the runtime.
        if let (Some(line), Some(spawner)) = (command_line, self.detached_spawner.clone()) {
            let mut req = orkia_shell_types::DetachedSpawnRequest::new(line);
            req.working_dir = self.agent_cwd().map(|d| d.display().to_string());
            req.agent_name = Some(agent_name.to_string());
            req.extra_env = self.rfc_scope_env();
            return match spawner.spawn_detached(req) {
                Ok(daemon_id) => Outcome::JobSpawned {
                    job_id: JobId(daemon_id),
                    foreground: false,
                    owner: JobOwner::Daemon,
                },
                Err(e) => Outcome::Error(format!("failed to spawn agent via daemon: {e}")),
            };
        }

        // Deliver the first prompt through the same detector-gated
        // path as follow-up `@agent <body>` calls
        // (`deliver_to_existing_agent` → `append_body`): the state
        // machine waits until the agent is actually idle at its input
        // prompt, then injects the body over the PTY.
        //
        // We deliberately do NOT use `StdinSource::InitialBytes` here,
        // even for hook-driven agents. Those bytes are written to the
        // PTY master microseconds after exec (see `job::spawn`), before
        // a TUI agent (claude) has entered raw mode or drawn its input
        // box — claude's startup swallows them and the prompt is
        // silently lost. Routing through the detector lands the body
        // once the agent is ready, regardless of provider.
        let stdin = orkia_shell_types::StdinSource::Pty;
        let pending_body = {
            let trimmed = body.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };

        let config = build_agent_job_config(AgentJobConfigInput {
            agent_name,
            cmd: &cmd,
            args: &args,
            extra_env,
            agent_context,
            hooks_provider: hooks_provider.as_deref(),
            stdin,
            pending_body,
            project: None,
            working_dir: self.agent_cwd(),
            cage_wrapper: self.cage_wrapper(agent_name),
        });
        let deps = crate::job::spawn::SpawnDeps {
            approvals: &self.approvals,
            event_router: &self.event_router,
            state_machine: &self.state_machine,
            injection_executor: &self.injection_executor,
            job_projects: &self.job_projects,
            agent_name,
        };
        match self.jobs.spawn(config, deps) {
            Ok(result) => {
                self.emit_public_job_on_spawn(result.job_id, agent_name, body);
                self.record_reasoning_scope(result.job_id, None).await;
                Outcome::JobSpawned {
                    job_id: result.job_id,
                    foreground: false,
                    owner: JobOwner::Local,
                }
            }
            Err(e) => Outcome::Error(format!("failed to spawn agent: {e}")),
        }
    }

    /// Stamp the per-job project/RFC into [`Self::reasoning_scopes`] so two
    /// consumers attribute this job correctly: the reasoning hot-path consumer
    /// (team/cloud) AND the operator fanout, which reads the same map to stamp
    /// `rfc_id` onto every hook event it grounds (`spawn_fanout` → `scope_for`).
    /// That second reader is why we do NOT gate on `intelligence`: an OSS user
    /// with an active `rfc cd` scope has no reasoning engine but still needs the
    /// operator to ground the agent (without this the agent's `hook.PreToolUse`
    /// seals carry no rfc_id → "no rfc_id" drift and no cross-session join).
    /// Still a no-op when there is nothing to attribute (no project, no RFC).
    /// The project name resolves to a UUID via the team cache (team/cloud only —
    /// `None` for OSS, which is normal). The RFC comes from the active `rfc cd`
    /// scope. `project_override` lets the RFC delegate path pass the RFC's own
    /// project explicitly.
    pub(crate) async fn record_reasoning_scope(
        &self,
        job_id: JobId,
        project_override: Option<&str>,
    ) {
        let project_name = project_override
            .map(str::to_string)
            .or_else(|| self.rfc_scope.as_ref().map(|s| s.project.clone()));
        let project_id = match project_name {
            Some(name) => self.team_cache.find_project(&name).await.map(|p| p.id),
            None => None,
        };
        let rfc_ref = self
            .rfc_scope
            .as_ref()
            .map(|s| orkia_reasoning_core::dto::RfcRef::new(s.rfc_id.clone()));
        if project_id.is_none() && rfc_ref.is_none() {
            return; // ad-hoc shell agent — leave the consumer on its fallback
        }
        if let Ok(mut map) = self.reasoning_scopes.write() {
            map.insert(
                job_id.0,
                orkia_kernel::JobScope {
                    project_id,
                    rfc_ref,
                },
            );
        }
    }

    /// Encode the active `rfc cd` scope as env vars for a daemon-owned spawn.
    /// A bare `@agent` dispatched under an `rfc cd` scope is daemon-owned
    /// (Phase 4): the daemon re-parses the forwarded line in a fresh runtime
    /// that holds no scope of its own. Without carrying the scope across, that
    /// runtime stamps no rfc_id onto the agent's `agent.spawn` genesis nor any
    /// `hook.PreToolUse` it seals — so the operator cannot ground the agent
    /// ("no rfc_id" drift) and the cross-session reconciler has no watch_paths
    /// to join against. The detached runtime adopts these back into `rfc_scope`
    /// via [`Repl::adopt_rfc_scope_from_env`] before it dispatches. Empty when
    /// no scope is active — a plain `orkia -c` from a user shell is unaffected.
    pub(crate) fn rfc_scope_env(&self) -> Vec<(String, String)> {
        self.rfc_scope
            .as_ref()
            .map(|s| {
                vec![
                    ("ORKIA_RFC_PROJECT".to_string(), s.project.clone()),
                    ("ORKIA_RFC_ID".to_string(), s.rfc_id.as_str().to_string()),
                ]
            })
            .unwrap_or_default()
    }

    /// `scope=public` project, publish the routing decision + the job. No-op
    /// for non-public projects. Reads the job's own UUID (`JobKind::Agent`) so
    /// `public_job.id` == the redactable event id.
    pub(crate) fn emit_public_job_on_spawn(&mut self, job_id: JobId, agent_name: &str, body: &str) {
        let Some(project) = self.public_project_for_cwd() else {
            return;
        };
        let Some(job_uuid) = self.jobs.agent_uuid(job_id) else {
            return;
        };
        let agent = self.agents.iter().find(|a| a.name == agent_name);
        let model = agent.map(|a| a.model.clone()).unwrap_or_default();
        let started_at = chrono::Utc::now().to_rfc3339();
        self.emit_routing_decided_local(super::emission::RoutingDecision {
            project: &project,
            agent: agent_name,
            model: &model,
            intent: body,
            job_uuid,
        });
        self.emit_public_job_spawned_local(
            &project,
            job_uuid,
            agent_name,
            &model,
            body,
            &started_at,
        );
        self.public_job_meta.insert(
            job_id,
            PublicJobMeta {
                project,
                agent_name: agent_name.to_string(),
                model,
                started_at,
                started: std::time::Instant::now(),
            },
        );
    }

    /// body (instruction + captured stdout), and spawn a fresh agent job. The
    /// composed body is delivered through the detector-gated `pending_body`
    /// channel (the state machine waits for the agent's ready prompt, then
    /// types + submits) — the same injection path as `dispatch_agent`, so a
    /// TUI agent receives it cleanly. Always a fresh spawn — the live-session
    /// reuse path used by `dispatch_agent` is intentionally skipped (a pipe is
    /// a transaction, not a continuation).
    pub(crate) async fn dispatch_shell_to_agent(
        &mut self,
        shell_cmd: &str,
        agent: &str,
        body: &str,
        command_line: Option<&str>,
    ) -> Outcome {
        // spawner is installed. Re-send the ORIGINAL command line so the detached
        // runtime re-parses + re-runs the WHOLE pipe (the shell stage included)
        // through the identical classifier — it re-derives capture, context, cage,
        // hooks, and stdin from its own config. A detached runtime has no spawner
        // (recursion guard) and falls through to the in-process spawn below.
        if let (Some(line), Some(spawner)) = (command_line, self.detached_spawner.clone()) {
            let mut req = orkia_shell_types::DetachedSpawnRequest::new(line);
            req.working_dir = self.agent_cwd().map(|d| d.display().to_string());
            req.agent_name = Some(agent.to_string());
            req.extra_env = self.rfc_scope_env();
            return match spawner.spawn_detached(req) {
                Ok(daemon_id) => Outcome::JobSpawned {
                    job_id: JobId(daemon_id),
                    foreground: false,
                    owner: JobOwner::Daemon,
                },
                Err(e) => Outcome::Error(format!("failed to spawn agent via daemon: {e}")),
            };
        }

        // 1. Snapshot brush env + cwd for the shell stage. We don't
        //    use brush.execute() here because that runs through the
        //    REPL PTY and the captured bytes would carry tty noise
        //    (echo, prompt sequences). `capture_shell_output` shells
        //    out to `sh -c` with stdin null + piped stdout, giving
        //    us clean bytes for the agent to consume.
        let (env, cwd) = {
            if let Err(e) = self.ensure_brush().await {
                return Outcome::Error(format!("shell engine init failed: {e}"));
            }
            let Some(brush_arc) = self.brush.clone() else {
                return Outcome::Error("shell engine unexpectedly absent".into());
            };
            let brush = brush_arc.lock().await;
            (brush.exported_env(), brush.cwd().to_path_buf())
        };

        // 2. Run the shell stage and capture stdout. Time it so the
        //    user can see how long the upstream took (matches the
        let started = std::time::Instant::now();
        let captured =
            match crate::shell_agent_pipe::capture_shell_output(shell_cmd, &env, &cwd).await {
                Ok(c) => c,
                Err(e) => return Outcome::Error(format!("shell stage failed to start: {e}")),
            };
        let elapsed_ms = started.elapsed().as_millis();

        //    (the user wants to see what went wrong), do not spawn
        //    the agent.
        if captured.exit_code != 0 {
            if !captured.stderr.is_empty() {
                self.emit_block(BlockContent::Text(
                    String::from_utf8_lossy(&captured.stderr).into_owned(),
                ));
            }
            return Outcome::Error(format!(
                "shell prefix failed (exit {}), agent not spawned",
                captured.exit_code
            ));
        }

        // 4. Surface stderr (when present) even on success — a
        //    well-behaved `cat file 2>>error.log` is fine, but agents
        //    benefit from seeing warnings the shell stage produced.
        //    We render stderr to the user, *not* to the agent
        //    (the agent only gets stdout per Unix convention).
        if !captured.stderr.is_empty() {
            self.emit_block(BlockContent::SystemInfo(format!(
                "shell stage stderr ({} bytes):",
                captured.stderr.len()
            )));
            self.emit_block(BlockContent::Text(
                String::from_utf8_lossy(&captured.stderr).into_owned(),
            ));
        }

        if captured.stdout_truncated {
            self.emit_block(BlockContent::SystemInfo(format!(
                "shell output truncated to {} bytes; agent receives only the first chunk",
                crate::shell_agent_pipe::MAX_CAPTURED_BYTES
            )));
        }

        //     so the user sees what was captured before the agent
        //     spawn block lands.
        self.emit_block(BlockContent::SystemInfo(format!(
            "▸ shell stage: {} bytes captured ({}ms)",
            captured.stdout.len(),
            elapsed_ms,
        )));

        // 6. Resolve the agent's runtime config. Unknown agent →
        //    short-circuit (no SEAL event, no spawn).
        let (cmd, args) = match self.config.resolve_agent(agent) {
            Some((cmd, args)) => (cmd.to_string(), args.to_vec()),
            None => {
                return Outcome::Error(format!(
                    "shell-to-agent pipe: {}",
                    self.config.agent_unresolved_reason(agent)
                ));
            }
        };

        // 7. Build the composed body (instruction + captured bytes
        let composed = crate::shell_agent_pipe::compose_body(body, &captured.stdout);
        let (agent_context, extra_env, hooks_provider) =
            self.build_agent_context(Some(agent)).await;

        // 8. SEAL event BEFORE the spawn so the per-job chain (created
        //    by the SealChain attachment on the next spawn) can be
        //    correlated with the originating shell command. Order
        let stdout_sha = short_sha(&captured.stdout);
        self.emit_audit_event(
            JobId(0),
            "",
            "shell.pipe.input",
            serde_json::json!({
                "agent": agent,
                "shell_cmd": shell_cmd,
                "bytes": captured.stdout.len(),
                "stdout_sha256": stdout_sha,
                "stdout_truncated": captured.stdout_truncated,
            }),
        );

        // 9. Spawn the agent. Deliver the composed body through the SAME
        //    detector-gated `pending_body` channel as `dispatch_agent` — NOT
        //    `StdinSource::InitialBytes`. InitialBytes writes to the PTY master
        //    microseconds after exec, before a TUI agent (claude) has entered
        //    raw mode or drawn its input box: the cooked tty echoes the bytes
        //    raw and the rest lands unsubmitted in the box (the exact failure
        //    `dispatch_agent` documents and avoids). The state machine instead
        //    waits for the agent's ready prompt, then types the body and
        //    submits it. A pipe is interactive, exactly like a single dispatch
        //    — one injection path, not two (non-negotiable #5).
        let stdin = orkia_shell_types::StdinSource::Pty;
        let composed_text = String::from_utf8_lossy(&composed).into_owned();
        let pending_body = {
            let trimmed = composed_text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };
        let config = build_agent_job_config(AgentJobConfigInput {
            agent_name: agent,
            cmd: &cmd,
            args: &args,
            extra_env,
            agent_context,
            hooks_provider: hooks_provider.as_deref(),
            stdin,
            pending_body,
            project: None,
            working_dir: self.agent_cwd(),
            cage_wrapper: self.cage_wrapper(agent),
        });
        let deps = crate::job::spawn::SpawnDeps {
            approvals: &self.approvals,
            event_router: &self.event_router,
            state_machine: &self.state_machine,
            injection_executor: &self.injection_executor,
            job_projects: &self.job_projects,
            agent_name: agent,
        };
        match self.jobs.spawn(config, deps) {
            Ok(result) => {
                self.record_reasoning_scope(result.job_id, None).await;
                Outcome::JobSpawned {
                    job_id: result.job_id,
                    foreground: false,
                    owner: JobOwner::Local,
                }
            }
            Err(e) => Outcome::Error(format!("failed to spawn agent: {e}")),
        }
    }

    /// Daemon flip for a verbatim REPL line: forward `line` to the
    /// daemon as a detached spawn for `agent` (the runtime re-parses it
    /// through the identical classifier → dispatch). `None` when no spawner
    /// is installed — a detached runtime (recursion guard); the caller falls
    /// back to its in-process spawn. Same shape as the inline flips in
    /// [`Self::dispatch_agent`] / [`Self::dispatch_shell_to_agent`].
    pub(super) fn spawn_detached_for_line(&self, line: &str, agent: &str) -> Option<Outcome> {
        let spawner = self.detached_spawner.clone()?;
        let mut req = orkia_shell_types::DetachedSpawnRequest::new(line);
        req.working_dir = self.agent_cwd().map(|d| d.display().to_string());
        req.agent_name = Some(agent.to_string());
        Some(match spawner.spawn_detached(req) {
            Ok(daemon_id) => Outcome::JobSpawned {
                job_id: JobId(daemon_id),
                foreground: false,
                owner: JobOwner::Daemon,
            },
            Err(e) => Outcome::Error(format!("failed to spawn agent via daemon: {e}")),
        })
    }
}

/// True when a daemon roster entry is a live, tellable session.
/// `pid_dead` / `control_unavailable` are corpses the daemon reaps at
/// list time — telling them yields "job N is stale"; `@agent` dispatch
/// must fall through to a fresh spawn instead. `lost_pty` (process
/// alive, control unreachable) stays live so the tell error surfaces
/// the anomaly rather than silently double-spawning.
pub(super) fn daemon_view_is_live(v: &orkia_shell_types::DaemonJobView) -> bool {
    !v.state.starts_with("done")
        && !v.state.starts_with("fail")
        && v.state != "pid_dead"
        && v.state != "control_unavailable"
}

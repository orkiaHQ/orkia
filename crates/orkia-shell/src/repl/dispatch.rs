// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

// arms below. Set-equality with `builtin_table::names_of_kind` is enforced by
// the exhaustiveness tests in `crate::builtin_table` — a name added to one
// side without the other fails the build's test gate (the anti-`top`,
// anti-`stream` guard).

/// Names served by [`Repl::dispatch_shell_control`].
pub(crate) const SHELL_CONTROL_ARMS: &[&str] = &[
    "fg", "bg", "stop", "kill", "run", "attach", "wait", "disown", "tui",
];

/// Names served by [`Repl::dispatch_auth_services`].
pub(crate) const AUTH_SERVICE_ARMS: &[&str] = &[
    "app",
    "login",
    "logout",
    "kernel",
    "reasoning",
    "contribute",
    "team",
    "invite",
    "members",
    "share",
    "leave",
    "stream",
];

/// Names served by [`Repl::dispatch_effectful`] (its `""` arm is the bare
/// `orkia` → help synthetic, not a table name). Test-only: `dispatch_named`
/// reaches this family by fall-through, so the const exists purely for the
/// exhaustiveness check.
#[cfg(test)]
pub(crate) const EFFECTFUL_ARMS: &[&str] = &[
    "plugin",
    "help",
    "ps",
    "detach",
    "approve",
    "deny",
    "audit",
    "rfc",
    "operator",
    "project",
    "issue",
    "agent",
    "cap",
    "trust",
    "config",
    "connect",
    "disconnect",
    "migrate-rc",
    "setup",
    "tell",
    "every",
];

impl Repl {
    pub(crate) async fn dispatch_shell(&mut self, cmd: &str, background: bool) -> Outcome {
        if let Err(e) = self.ensure_brush().await {
            return Outcome::Error(format!("shell engine init failed: {e}"));
        }
        // Safe: ensure_brush() guaranteed Some(_) above. Using `if let Some`
        // rather than `unwrap` to keep the no-panic policy.
        let Some(brush_arc) = self.brush.clone() else {
            return Outcome::Error("shell engine unexpectedly absent".into());
        };
        let mut brush = brush_arc.lock().await;

        if background {
            // `cmd &` path. Expand the command line through brush
            // (preserving `$VAR` / globs / tilde semantics) so the
            // spawn-time argv matches what `bash -c cmd` would see;
            // then hand off to `JobController::spawn_shell` to
            // launch a PTY-backed child in its own session. No agent
            // attachments (hooks, state machine, SEAL chain, …) —
            // shell jobs are plain Unix jobs the user controls via
            // jobs / fg / bg / wait / kill %N.
            //
            // Lines with shell metacharacters (`|`, `;`, `&&`,
            // `||`, `(`) can't be directly expanded into a single
            // argv — they need a shell to interpret. Route those
            // through `sh -c '<cmd>'`. Loses brush aliases /
            // functions in the backgrounded context (sh doesn't
            // see them) but supports pipelines, sequencing, etc.
            let argv: Vec<String> = if cmd_contains_shell_operators(cmd) {
                vec!["sh".into(), "-c".into(), cmd.to_string()]
            } else {
                match brush.expand_to_argv(cmd).await {
                    Ok(v) if !v.is_empty() => v,
                    Ok(_) => return Outcome::Error("empty command".into()),
                    Err(e) => return Outcome::Error(format!("expand: {e}")),
                }
            };
            let env = brush.exported_env();
            let cwd = Some(brush.cwd().to_path_buf());
            let label = cmd.trim().to_string();
            drop(brush);
            let data_dir = self.config.data_dir.clone();
            match self.jobs.spawn_shell(&argv, env, cwd, label, &data_dir) {
                Ok(job_id) => {
                    // Bash sets `$!` to the pid of the most-recent
                    // backgrounded process. Honour the convention so
                    // `cmd & echo $!` works as scripts expect.
                    if let Some(pid) = self.jobs.get(job_id).and_then(|e| e.pid()) {
                        let mut brush = brush_arc.lock().await;
                        brush.set_last_bg_pid(pid);
                    }
                    Outcome::JobSpawned {
                        job_id,
                        foreground: false,
                        owner: orkia_shell_types::JobOwner::Local,
                    }
                }
                Err(e) => Outcome::Error(format!("spawn: {e}")),
            }
        } else {
            // line before this command runs, so `echo $?` / `[ $? -eq 0 ]`
            // see a builtin's code across the builtin/brush boundary.
            // A prior brush line re-seeds its own code — a no-op. Brush
            // then overwrites `$?` with this command's result naturally.
            brush.set_last_status(u8::try_from(self.last_outcome_status).unwrap_or(1));
            match brush.execute(cmd).await {
                Ok(out) => {
                    if out.should_exit {
                        self.should_exit = true;
                    }
                    // Refresh the cwd cache before releasing the brush lock
                    // — the command may have been a `cd`. The next prompt
                    // reads the cache without re-locking.
                    let new_cwd = brush.cwd().to_path_buf();
                    drop(brush);
                    self.cwd_cache = Some(new_cwd);
                    // For now the brush PTY's raw bytes (including any ANSI
                    // colour escapes from the child) are surfaced as a single
                    // Text block. Block-segmenting from the OSC-133 markers
                    // we emit will hook into the renderer separately.
                    Outcome::ShellComplete {
                        exit_code: out.exit_code as i32,
                        output: String::from_utf8_lossy(&out.bytes).into_owned(),
                    }
                }
                Err(e) => Outcome::Error(format!("{e}")),
            }
        }
    }

    /// Team coordinator if wired; otherwise return a clear
    /// Team-required error. The coordinator itself launches the run
    /// on its own task and reports back via `PipelineProgressEvent`
    /// callbacks; this method only validates pre-flight + returns
    /// the launched/refused outcome.
    pub(crate) async fn dispatch_pipeline(&mut self, stages: &[PipelineStage]) -> Outcome {
        let Some(coord) = self.pipeline_coordinator.clone() else {
            return Outcome::Error(
                "@a | @b requires Orkia Team. See https://orkia.dev/team".into(),
            );
        };
        // forgeable locally; require server-side team membership as
        // the source of truth before launching the coordinator.
        match self.team_client.me().await {
            Ok(me) => {
                if me.teams.is_empty() {
                    tracing::warn!(
                        "TeamPipeline gate: caller has no team membership; refusing pipeline"
                    );
                    return Outcome::Error(
                        "Agent pipelines require team membership. Join a team first.".into(),
                    );
                }
            }
            Err(orkia_shell_types::TeamClientError::Unavailable { .. }) => {
                // OSS build / no backend wired — keep historical
                // "no coordinator → no pipeline" behavior the
                // shell already enforced above. We fall through
                // to dispatch; the coordinator that *is* wired
                // here clearly came from a richer build and made
                // its own decision to allow pipelines.
            }
            Err(e) => {
                tracing::warn!(error = ?e, "TeamPipeline gate: me() failed; refusing pipeline");
                return Outcome::Error(format!(
                    "Could not verify team membership: {}",
                    orkia_shell_types::team_error_message(&e)
                ));
            }
        }
        let request = orkia_shell_types::AgentPipelineRequest::AgentChain {
            stages: stages
                .iter()
                .map(|s| orkia_shell_types::AgentPipelineStage {
                    agent: s.agent.clone(),
                    body: s.body.clone(),
                })
                .collect(),
        };
        match coord.dispatch(request).await {
            orkia_shell_types::PipelineDispatchOutcome::Launched {
                pipeline_id,
                total_stages,
            } => Outcome::PipelineStarted {
                stages: (0..total_stages)
                    .map(|i| format!("{pipeline_id}#{i}"))
                    .collect(),
            },
            orkia_shell_types::PipelineDispatchOutcome::Refused { reason } => {
                Outcome::Error(format!("pipeline refused: {reason}"))
            }
        }
    }

    /// and host state into a read-only `CommandCtx`, materializes an optional
    /// external shell prefix into a `ByteStream` (the `Bytes → Value`
    /// boundary), runs the streaming engine, then renders the result through
    /// the unchanged `emit_block` path. The REPL only blocks here inside the
    /// existing async dispatch — never on anything but `readline`.
    pub(crate) async fn dispatch_exec(&mut self, plan: &orkia_shell_types::ExecPlan) -> Outcome {
        use crate::exec::engine::{PipelineInput, run_plan};
        use futures::stream::StreamExt;
        use orkia_shell_types::exec::command::CommandCtx;
        use orkia_shell_types::exec::pipeline_data::PipelineData;

        if let Err(e) = self.ensure_brush().await {
            return Outcome::Error(format!("shell engine init failed: {e}"));
        }
        let Some(brush_arc) = self.brush.clone() else {
            return Outcome::Error("shell engine unexpectedly absent".into());
        };
        let (env, cwd) = {
            let brush = brush_arc.lock().await;
            (brush.exported_env(), brush.cwd().to_path_buf())
        };

        // An external prefix (`echo … | from json | …`) is captured as bytes —
        // the only entry into structured data, per the fail-closed boundary.
        let (data, label): (PipelineData, String) = match &plan.shell_prefix {
            Some(prefix) => {
                match crate::shell_agent_pipe::capture_shell_output(prefix, &env, &cwd).await {
                    Ok(captured) => {
                        let bytes = bytes::Bytes::from(captured.stdout);
                        let stream = futures::stream::once(async move { Ok(bytes) }).boxed();
                        (PipelineData::ByteStream(stream), prefix.clone())
                    }
                    Err(e) => return Outcome::Error(format!("shell stage failed: {e}")),
                }
            }
            None => (PipelineData::Empty, "input".to_string()),
        };

        let auth_view = crate::auth_builtins::ShellAuthView {
            auth: self.auth_provider.clone(),
            resolver: self.capability_resolver.clone(),
            adaptive: self.adaptive_handle.clone(),
        };
        let attention = self.attention.rows();
        let ctx = CommandCtx {
            cwd: cwd.clone(),
            env: env.iter().cloned().collect(),
            data_dir: self.config.data_dir.clone(),
            agents: self.agents.clone(),
            jobs: self.jobs.list(),
            journal: self.journal_envelope_hook.clone(),
            auth: Some(std::sync::Arc::new(auth_view)),
            attention,
            attention_control: Some(std::sync::Arc::new(self.attention.clone())),
            // Native builtins are trusted Orkia code: grant the default shell
            // capability set (FS + env + clock + randomness; no network).
            capabilities: orkia_shell_types::CapabilitySet::shell_default(),
        };

        let input = PipelineInput { data, label };
        let result = match run_plan(&plan.stages, input, &ctx, &self.registry).await {
            Ok(r) => r,
            // type/conversion refusal) report 2, runtime failures 1 —
            // the split lives in `ExecError::exit_code`.
            Err(e) if e.exit_code() == 2 => return Outcome::UsageError(e.to_string()),
            Err(e) => return Outcome::Error(e.to_string()),
        };

        // streaming it line by line — never `into_value` here.
        match &plan.external_suffix {
            // Value → Bytes hand-off: stream into the external command's stdin.
            Some(suffix) => {
                let sink = crate::sink::ExternalSink {
                    command: suffix.clone(),
                    env,
                    cwd,
                };
                match sink.drive(result).await {
                    Ok(output) => Outcome::BuiltinOutput {
                        blocks: output.into_blocks(),
                    },
                    Err(e) => Outcome::Error(e.to_string()),
                }
            }
            // Incremental display: emit aligned chunks as rows arrive.
            None => match self.emit_display_stream(result).await {
                Ok(()) => Outcome::Noop,
                Err(e) => Outcome::Error(e.to_string()),
            },
        }
    }

    /// Stream a typed result to the display incrementally: rows are emitted in
    /// aligned chunks (Policy C, size `DISPLAY_CHUNK_ROWS`) **or** every
    /// whichever comes first. A small/fast result still emits as one chunk.
    /// Never calls `into_value`.
    pub(crate) async fn emit_display_stream(
        &mut self,
        data: orkia_shell_types::exec::pipeline_data::PipelineData,
    ) -> Result<(), orkia_shell_types::ExecError> {
        use futures::stream::StreamExt;
        use orkia_shell_types::exec::pipeline_data::PipelineData;

        match data {
            PipelineData::Empty => {}
            PipelineData::Value(value) => {
                for block in crate::exec::display::value_to_blocks(&value) {
                    self.emit_block(block);
                }
            }
            PipelineData::ByteStream(mut stream) => {
                let mut buf = Vec::new();
                while let Some(chunk) = stream.next().await {
                    buf.extend_from_slice(&chunk?);
                }
                let text = String::from_utf8_lossy(&buf);
                let text = text.trim_end_matches('\n');
                if !text.is_empty() {
                    self.emit_block(BlockContent::Text(text.to_string()));
                }
            }
            PipelineData::ListStream(stream) => {
                // Size flush (Policy C) OR temporal flush (slow producers) —
                // sink (C1): `drive_list_stream` calls back per aligned block.
                crate::sink::drive_list_stream(
                    stream,
                    crate::sink::DISPLAY_FLUSH_INTERVAL,
                    |block| self.emit_block(block),
                )
                .await?;
            }
        }
        Ok(())
    }

    /// for everything that is NOT a registry `Command`. The second of the two
    /// surviving dispatch paths (the first is the `Command` registry); the
    /// `enum BuiltinCmd` and its two giant match blocks are gone.
    ///
    /// These builtins either **drive the REPL** (job control, attach/detach,
    /// TUI) or **mutate REPL-owned state / perform privileged effects** (auth,
    /// RFC, team, tell, app, kernel, …). They need `&mut self`, so they are NOT
    /// read-only pipeline `Command`s and never touch the registry. Per-command
    /// argument parsing lives here. `ps` (POSIX collision — bare `ps` isn't
    /// typed) and `help` (the bare-`orkia` landing the seam can't catch) are
    /// kept here; every other read-only builtin is registry-only.
    pub(crate) async fn dispatch_named(&mut self, name: &str, args: &[String]) -> Outcome {
        // Fast exit for shell-control builtins (all return directly).
        if SHELL_CONTROL_ARMS.contains(&name) {
            return self.dispatch_shell_control(name, args).await;
        }
        // Fast exit for auth/service builtins (all return directly).
        if AUTH_SERVICE_ARMS.contains(&name) {
            return self.dispatch_auth_services(name, args).await;
        }
        // Remaining effectful builtins.
        self.dispatch_effectful(name, args).await
    }

    /// Effectful builtins: mutate REPL-owned state / privileged effects.
    async fn dispatch_effectful(&mut self, name: &str, args: &[String]) -> Outcome {
        let blocks = match name {
            // ── synthetics / read-only kept on this path ──
            "plugin" => return self.handle_plugin(args),
            "" | "help" => orkia_builtin::help::help(),
            "ps" => match PsFlags::parse(args) {
                Ok(flags) => orkia_builtin::ps::render(&self.build_ps_model(), &flags),
                Err(e) => return Outcome::UsageError(e),
            },
            "detach" => vec![BlockContent::SystemInfo(
                "detach: only available from foreground".into(),
            )],
            "approve" => return self.handle_resolution(args, true),
            "deny" => return self.handle_resolution(args, false),
            "audit" if args.first().map(String::as_str) == Some("redact") => {
                return self.handle_audit_redact(args.get(1..).unwrap_or_default());
            }
            "audit" => crate::seal::render_audit(&self.config.data_dir, args),
            "rfc" => return self.handle_rfc(args).await,
            "operator" => return self.handle_operator(args).await,
            "project" => self.handle_project(args),
            "issue" => self.handle_issue(args),
            "agent" => self.handle_agent(args),
            "cap" => return self.handle_cap(args),
            "trust" => return self.handle_trust(args),
            "config" => self.handle_config(args),
            "connect" => connect_blocks(args, self.connected),
            "disconnect" => vec![BlockContent::SystemInfo("disconnect: nothing to do".into())],
            "migrate-rc" => return self.handle_migrate_rc(args),
            "setup" => return self.handle_setup(args).await,
            "tell" => {
                let Some(target) = args.first().cloned() else {
                    return Outcome::Error("tell: missing target".into());
                };
                let message = args.get(1..).unwrap_or_default().join(" ");
                if message.trim().is_empty() {
                    return Outcome::Error("tell: missing message".into());
                }
                return self.handle_tell(&target, &message);
            }
            "every" => orkia_builtin::every::every(args, &self.config.data_dir),
            // child orkia is ever spawned through brush.
            other => return Outcome::Error(crate::builtin_table::unknown_builtin_message(other)),
        };
        Outcome::BuiltinOutput { blocks }
    }

    /// Shell-control builtins: all return an `Outcome` directly.
    async fn dispatch_shell_control(&mut self, name: &str, args: &[String]) -> Outcome {
        match name {
            "fg" => self.handle_fg(args.first().map(String::as_str)).await,
            "bg" => self.handle_bg_target(args.first().map(String::as_str)),
            "stop" => self.handle_stop_target(args.first().map(String::as_str).unwrap_or("")),
            "kill" => match split_kill_args(args) {
                Ok((signal, target)) => self.handle_kill(&target, signal.as_deref()),
                Err(e) => Outcome::UsageError(e),
            },
            "run" => {
                let cmd = args.first().cloned().unwrap_or_default();
                self.handle_run(&cmd, args.get(1..).unwrap_or_default())
                    .await
            }
            "attach" => {
                let target = args.first().cloned().unwrap_or_default();
                self.handle_attach(&target).await
            }
            "wait" => self.handle_wait(args.first().map(String::as_str)).await,
            "disown" => self.handle_disown(args.first().map(String::as_str)),
            "tui" => self.handle_tui().await,
            other => Outcome::Error(format!("unknown builtin: {other}")),
        }
    }

    /// Auth/service builtins: all return an `Outcome` directly.
    async fn dispatch_auth_services(&mut self, name: &str, args: &[String]) -> Outcome {
        match name {
            "app" => self.handle_app(args).await,
            "login" => {
                let mut blocks = crate::auth_builtins::login(
                    self.auth_provider.as_ref(),
                    self.capability_resolver.as_ref(),
                    self.adaptive_handle.as_ref(),
                )
                .await;
                // The gate may have just opened (premium session) — (re-)boot
                // the reasoning hot path.
                // No-op when still anonymous/free (`boot_intelligence` is gated).
                self.reboot_intelligence();
                self.note_pipeline_restart_if_pending(&mut blocks);
                Outcome::BuiltinOutput { blocks }
            }
            "logout" => {
                // Tear the reasoning tasks down before the session clears so the
                // store connections close cleanly. Idempotent.
                if let Some(intel) = self.intelligence.as_mut() {
                    intel.shutdown();
                }
                self.intelligence = None;
                Outcome::BuiltinOutput {
                    blocks: crate::auth_builtins::logout(
                        self.auth_provider.as_ref(),
                        self.capability_resolver.as_ref(),
                        self.adaptive_handle.as_ref(),
                    )
                    .await,
                }
            }
            "kernel" => Outcome::BuiltinOutput {
                blocks: crate::kernel_builtins::dispatch(
                    args,
                    self.auth_provider.as_ref(),
                    self.capability_resolver.as_ref(),
                    self.adaptive_handle.as_ref(),
                )
                .await,
            },
            "reasoning" => self.handle_reasoning(args).await,
            "contribute" => Outcome::BuiltinOutput {
                blocks: crate::contribute_builtins::dispatch(args),
            },
            "team" => self.handle_team(args).await,
            "invite" => self.handle_invite(args).await,
            "members" => self.handle_members(args).await,
            "share" => self.handle_share(args).await,
            "leave" => self.handle_leave(args).await,
            "stream" => self.handle_stream(args),
            other => Outcome::Error(format!("unknown builtin: {other}")),
        }
    }

    /// After `$login`, the plan may now grant `TeamPipeline` and the
    /// kernel may have just been provisioned — but the `@a | @b`
    /// coordinator is wired at journal-boot (the journal listener owns
    /// the stage envelope + stop hooks and starts once), so it cannot
    /// attach live this session. Surface a one-line restart hint instead
    /// of silently leaving `@a | @b` returning "requires Orkia Team".
    /// No-op once a coordinator is already attached (returning Team
    /// users, where the build-time wiring caught it).
    fn note_pipeline_restart_if_pending(&self, blocks: &mut Vec<BlockContent>) {
        if self.pipeline_coordinator.is_some() {
            return;
        }
        let grants_pipeline = self
            .capability_resolver
            .as_ref()
            .map(|r| {
                r.current()
                    .has(orkia_capabilities::Capability::TeamPipeline)
            })
            .unwrap_or(false);
        if grants_pipeline {
            blocks.push(BlockContent::Text(
                "  ▸ restart orkia to activate team pipelines (@a | @b)".into(),
            ));
        }
    }
}

/// Build the blocks for the `connect` builtin. Standalone so `dispatch_effectful`
/// stays within the 50-line limit.
fn connect_blocks(args: &[String], connected: bool) -> Vec<BlockContent> {
    let url = args.first().cloned().unwrap_or_default();
    if url.is_empty() {
        vec![BlockContent::SystemInfo(format!(
            "connect: {}",
            if connected {
                "connected"
            } else {
                "not connected"
            }
        ))]
    } else {
        vec![BlockContent::SystemInfo(format!(
            "connect {url}: backend sync not yet available"
        ))]
    }
}

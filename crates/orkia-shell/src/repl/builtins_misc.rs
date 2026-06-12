// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

impl Repl {
    /// Write the user's message into the target agent's PTY. Resolves
    /// `target` through the same matcher `fg`/`bg` use (numeric id or
    /// agent name). Emits a `Tell` journal envelope so the message
    /// shows up in `journal --type tell` and the SEAL chain.
    pub(crate) fn handle_tell(&mut self, target: &str, message: &str) -> Outcome {
        let jobs = self.jobs.list();
        let id = match resolve_job_target(target, &jobs) {
            Some(id) => id,
            // Daemon fallback: a detached job surviving a REPL restart is owned
            None => match self.resolve_daemon_target(target) {
                Some(did) => return self.tell_daemon_job(did, message),
                None => return Outcome::Error(format!("tell: no such job: {target}")),
            },
        };
        // Native session: no PTY to write — enqueue the body on the
        // actor's control channel instead. Same Tell envelope so the
        // journal/SEAL surface is identical.
        let native_delivery = self
            .jobs
            .native_inbound(id)
            .map(|tx| tx.send(crate::native::NativeSessionMsg::User(message.to_string())));
        if let Some(sent) = native_delivery {
            if sent.is_err() {
                return Outcome::Error(format!("tell: native session [{}] is gone", id.0));
            }
            let mut env = JournalEnvelope::now(EventType::Tell);
            env.job_id = Some(id.0);
            env.source = Some("orkia".into());
            env.message = Some(message.to_string());
            self.emit_journal(env);
            return Outcome::BuiltinOutput {
                blocks: vec![BlockContent::SystemInfo(format!(
                    "tell: delivered to job {id}"
                ))],
            };
        }
        // The paced, byte-by-byte PTY write (with a trailing `\r`) runs on the
        // `InjectionExecutor` thread — the same path `emit_injection` uses.
        // The old inline loop slept 5ms PER BYTE on the REPL thread, freezing
        // the shell for ~5*N ms (≈1s for 200 chars) — a direct violation of
        // "the REPL loop is sacred / never block" (BUG-038).
        let agent_name = jobs
            .iter()
            .find(|j| j.id == id)
            .and_then(|j| match &j.kind {
                JobKind::Agent { agent_name, .. } => Some(agent_name.clone()),
                _ => None,
            })
            .unwrap_or_default();
        self.injection_executor.inject(id, &agent_name, message);
        let mut env = JournalEnvelope::now(EventType::Tell);
        env.job_id = Some(id.0);
        env.source = Some("orkia".into());
        env.message = Some(message.to_string());
        self.emit_journal(env);
        Outcome::BuiltinOutput {
            blocks: vec![BlockContent::SystemInfo(format!(
                "tell: delivered to job {id}"
            ))],
        }
    }

    /// Entry point for the `tui` builtin. Swaps the active renderer for a
    /// fresh `TuiRenderer`, runs the same REPL loop body until the user
    /// leaves the TUI (Ctrl-Z detach / child exits the ratatui sub-loop /
    /// stdin EOF), then restores the prior renderer and returns control
    /// to the shell-mode prompt.
    pub(crate) async fn handle_tui(&mut self) -> Outcome {
        // Construct the TUI renderer via the injected factory. The binary
        // wires this on startup so orkia-shell stays decoupled from
        // orkia-shell-tui.
        let Some(factory) = self.tui_factory.as_mut() else {
            return Outcome::Error(
                "tui: renderer not wired (binary must install a TuiFactory)".into(),
            );
        };
        let tui = match factory(&self.agents, &self.workspace) {
            Ok(r) => r,
            Err(e) => return Outcome::Error(format!("failed to init TUI: {e}")),
        };

        // Take the shell-mode renderer out; we'll restore it on exit.
        // `Box<dyn ShellRenderer>` is `Send + 'static`, so this is just
        // a pointer swap.
        let prior = std::mem::replace(&mut self.renderer, tui);

        // Paint recent context into the TUI before its first frame.
        for block in self.recent_blocks.iter().cloned().collect::<Vec<_>>() {
            self.renderer.publish(RenderEvent::Block(block));
        }
        self.emit_workspace_snapshot();
        self.emit_jobs_snapshot();
        self.render_welcome();

        // Run the inner loop. The renderer is the TuiRenderer; the rest
        // of the REPL state (engine, jobs, seal, workspace) is unchanged.
        // Breaks on `should_exit` (set by `exit` or by the TUI surface
        // returning `read_line == None`).
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
            let ctx = self.prompt_context();
            let line = match self.renderer.read_line(&ctx) {
                Some(line) => line,
                None => break,
            };
            if let Err(e) = Box::pin(self.tick(line)).await {
                self.renderer
                    .publish(RenderEvent::Block(BlockContent::Error(format!("{e}"))));
            }
        }

        // Restore. The TuiRenderer's Drop releases the alternate screen.
        self.renderer = prior;
        // Don't propagate should_exit beyond the TUI sub-loop: the user
        // may have just typed `exit` to leave the TUI, not to quit orkia.
        // If they want to quit, they can `exit` again from shell mode.
        // (Exception: if stdin EOF'd in the TUI we still want to exit —
        // this case is indistinguishable from "user pressed `q`" and is
        // handled by the outer loop's normal None-from-read_line break.)
        let leaving_via_exit = self.should_exit;
        self.should_exit = false;
        if leaving_via_exit {
            // Re-render the prompt context so the shell-mode renderer
            // can paint its prompt at the bottom of the restored screen.
        }
        // through `render_outcome` like any other, so `last_outcome_status`
        // already holds the last in-TUI command's code. Returning Noop here
        // would clobber it back to 0 — carry it instead so
        // `tui … <failing cmd> … exit; echo $?` stays truthful.
        Outcome::ShellComplete {
            exit_code: self.last_outcome_status,
            output: String::new(),
        }
    }

    /// `orkia migrate-rc [--from PATH] [--dry-run] [--append]`.
    /// Pure-by-design: only reads the source rc and (optionally) writes
    /// `~/.orkiarc`. Never touches the source file. Delegates to the
    /// shared `orkia_builtin::migrate_rc::run_migration` so the same
    /// behaviour is available from the CLI subcommand.
    pub(crate) fn handle_migrate_rc(&mut self, args: &[String]) -> Outcome {
        let opts = match orkia_builtin::migrate_rc::MigrateRcOpts::parse(args) {
            Ok(o) => o,
            Err(e) => return Outcome::Error(format!("migrate-rc: {e}")),
        };
        let Some(home) = dirs_home() else {
            return Outcome::Error("migrate-rc: HOME not set; pass --from explicitly".into());
        };
        let dest = home.join(".orkiarc");
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let report = match orkia_builtin::migrate_rc::run_migration(&opts, &home, &dest, &today) {
            Ok(r) => r,
            Err(e) => return Outcome::Error(format!("migrate-rc: {e}")),
        };

        let mut blocks = build_migration_summary(&report);
        if opts.dry_run {
            blocks.push(BlockContent::SystemInfo(
                "dry-run: nothing written. Re-run without --dry-run to commit.".into(),
            ));
            blocks.push(BlockContent::Text(report.orkiarc_body));
        } else if let Some(p) = &report.written_to {
            blocks.push(BlockContent::SystemInfo(format!(
                "✓ written to {}",
                p.display()
            )));
        }
        if let Some(err) = &report.write_error {
            blocks.push(BlockContent::Error(err.clone()));
        }
        Outcome::BuiltinOutput { blocks }
    }

    /// `setup` — re-exec this orkia binary as `orkia setup [args...]` so
    /// the wizard owns stdin/stderr cleanly. We yield the terminal,
    /// inherit fds, wait for the child, then reclaim. PATH-independent:
    /// we use `std::env::current_exe()` so it works even when the user
    /// has disabled bashrc and `~/.cargo/bin` isn't in PATH.
    pub(crate) async fn handle_setup(&mut self, args: &[String]) -> Outcome {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => return Outcome::Error(format!("setup: locate orkia binary: {e}")),
        };
        self.renderer.yield_terminal();
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("setup").args(args);
        // Inherit stdin/stdout/stderr so the wizard's prompts and the
        // user's keystrokes flow directly.
        let status = cmd.status();
        self.renderer.reclaim_terminal();

        // Wizard may have scaffolded new agents/projects on disk; pull
        // them into the running REPL so subsequent commands see them
        // without requiring a shell restart.
        self.refresh_workspace_state();

        match status {
            Ok(s) if s.success() => Outcome::BuiltinOutput {
                blocks: vec![BlockContent::SystemInfo(format!(
                    "setup: complete. {} agent{} loaded.",
                    self.agents.len(),
                    if self.agents.len() == 1 { "" } else { "s" },
                ))],
            },
            Ok(s) => Outcome::Error(format!(
                "setup: orkia wizard exited with status {}",
                s.code().unwrap_or(-1)
            )),
            Err(e) => Outcome::Error(format!("setup: spawn {}: {e}", exe.display())),
        }
    }

    /// Re-hydrate REPL state from disk after an external mutator
    /// (currently `setup`, but also fits future `agent create` / `project
    /// add`) has changed `~/.orkia/`. Refreshes:
    ///
    /// * `self.config` — picks up newly-scaffolded `~/.orkia/agents/*`
    /// * `self.agents` — the cached AgentInfo vec the classifier uses
    /// * `self.workspace` — projects/issues/rfcs
    ///
    /// Snapshots get re-emitted so any active renderer (sidebar, etc.)
    /// repaints with the fresh state.
    pub(crate) fn refresh_workspace_state(&mut self) {
        self.config = ShellConfig::load();
        self.agents = self.config.agents.clone();
        self.workspace = Workspace::load(&self.config.data_dir);
        self.emit_workspace_snapshot();
    }

    /// `audit redact <event_id> [--reason "<text>"]` — publish an append-only
    /// rewritten; the backend masks the target entity's public projection.
    pub(crate) fn handle_audit_redact(&mut self, args: &[String]) -> Outcome {
        let mut event_id: Option<&str> = None;
        let mut reason: Option<String> = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--reason" => {
                    reason = args.get(i + 1).cloned();
                    i += 2;
                }
                other if event_id.is_none() => {
                    event_id = Some(other);
                    i += 1;
                }
                _ => i += 1,
            }
        }
        let Some(event_id) = event_id else {
            return Outcome::UsageError(
                "usage: audit redact <event_id> [--reason \"<text>\"]".into(),
            );
        };
        if uuid::Uuid::parse_str(event_id).is_err() {
            return Outcome::BuiltinOutput {
                blocks: vec![BlockContent::Error(format!(
                    "audit redact: `{event_id}` is not a valid event id (UUID)"
                ))],
            };
        }
        self.emit_redact_event_local(event_id, reason.as_deref());
        Outcome::BuiltinOutput {
            blocks: vec![BlockContent::SystemInfo(format!(
                "redaction published for {event_id} — the public projection will be masked; \
                 the SEAL chain is unchanged"
            ))],
        }
    }

    pub(crate) fn handle_project(&mut self, args: &[String]) -> Vec<BlockContent> {
        use orkia_builtin::project::{self, ProjectAction};
        use orkia_builtin::scope_validation::validate_artifact_scope;
        match project::parse(args) {
            Ok(ProjectAction::List) => project::list(&self.workspace),
            Ok(ProjectAction::Show { name }) => project::show(&self.workspace, &name),
            Ok(ProjectAction::Create {
                name,
                description,
                scope,
            }) => {
                if let Some(s) = scope
                    && let Err(e) =
                        validate_artifact_scope(&self.config.data_dir, &self.workspace, None, s)
                {
                    return vec![BlockContent::Error(e)];
                }
                let mut blocks =
                    project::create(&self.workspace.root, &name, description.as_deref(), scope);
                self.workspace.reload();
                self.emit_workspace_snapshot();
                if let Some(scope) = scope {
                    // Newly-created project — no `previous` value.
                    self.emit_scope_event_local(
                        "project.scope_set",
                        Some(&name),
                        &name,
                        None,
                        scope,
                    );
                }
                blocks.extend(self.maybe_warn_team_scope(scope, &name));
                blocks
            }
            Ok(ProjectAction::Update {
                name,
                description,
                scope,
            }) => {
                if let Some(s) = scope
                    && let Err(e) =
                        validate_artifact_scope(&self.config.data_dir, &self.workspace, None, s)
                {
                    return vec![BlockContent::Error(e)];
                }
                // Snapshot the previous scope before mutating so the SEAL
                // event carries an honest before/after.
                let previous_scope = self.workspace.project(&name).and_then(|p| p.scope);
                let mut blocks =
                    project::update(&self.workspace, &name, description.as_deref(), scope);
                self.workspace.reload();
                self.emit_workspace_snapshot();
                if let Some(scope) = scope
                    && previous_scope != Some(scope)
                {
                    self.emit_scope_event_local(
                        "project.scope_changed",
                        Some(&name),
                        &name,
                        previous_scope,
                        scope,
                    );
                }
                blocks.extend(self.maybe_warn_team_scope(scope, &name));
                blocks
            }
            Err(e) => vec![BlockContent::Error(e)],
        }
    }

    pub(crate) fn handle_issue(&mut self, args: &[String]) -> Vec<BlockContent> {
        use orkia_builtin::issue::{self, IssueAction};
        use orkia_builtin::scope_validation::validate_artifact_scope;
        match issue::parse(args) {
            Ok(IssueAction::List { project }) => {
                orkia_builtin::issue::list(&self.workspace, project.as_deref())
            }
            Ok(IssueAction::Create {
                title,
                project,
                priority,
                scope,
            }) => {
                if let Some(s) = scope
                    && let Err(e) = validate_artifact_scope(
                        &self.config.data_dir,
                        &self.workspace,
                        Some(&project),
                        s,
                    )
                {
                    return vec![BlockContent::Error(e)];
                }
                let mut blocks = issue::create(&self.workspace, &project, &title, &priority, scope);
                self.workspace.reload();
                self.emit_workspace_snapshot();
                if let Some(scope) = scope {
                    let artifact_id = format!("{project}/{title}");
                    self.emit_scope_event_local(
                        "issue.scope_set",
                        Some(&project),
                        &artifact_id,
                        None,
                        scope,
                    );
                    blocks.extend(self.maybe_warn_team_scope(Some(scope), &artifact_id));
                }
                blocks
            }
            Ok(IssueAction::Update {
                number,
                project,
                field,
                value,
            }) => {
                if field == "scope"
                    && let Ok(proposed) = orkia_shell_types::Scope::parse(&value)
                    && let Err(e) = validate_artifact_scope(
                        &self.config.data_dir,
                        &self.workspace,
                        Some(&project),
                        proposed,
                    )
                {
                    return vec![BlockContent::Error(e)];
                }
                // Snapshot the previous scope when this is a scope edit.
                let previous_scope = if field == "scope" {
                    self.workspace
                        .project(&project)
                        .and_then(|p| {
                            p.issues
                                .iter()
                                .find(|i| i.number == number)
                                .map(|i| i.scope)
                        })
                        .flatten()
                } else {
                    None
                };
                let blocks = issue::update(&self.workspace, &project, number, &field, &value);
                self.workspace.reload();
                self.emit_workspace_snapshot();
                if field == "scope"
                    && let Ok(parsed) = orkia_shell_types::Scope::parse(&value)
                    && previous_scope != Some(parsed)
                {
                    let artifact_id = format!("{project}/#{number}");
                    self.emit_scope_event_local(
                        "issue.scope_changed",
                        Some(&project),
                        &artifact_id,
                        previous_scope,
                        parsed,
                    );
                }
                blocks
            }
            Err(e) => vec![BlockContent::Error(e)],
        }
    }

    pub(crate) fn handle_config(&mut self, args: &[String]) -> Vec<BlockContent> {
        use orkia_builtin::config::{self, ConfigAction};
        let action = match config::parse(args) {
            Ok(a) => a,
            Err(e) => return vec![BlockContent::Error(e)],
        };
        // Snapshot whether this is a default_scope mutation before
        // dispatching so we can decide whether to emit a SEAL event.
        let is_default_scope_set =
            matches!(&action, ConfigAction::Set { key, .. } if key == "default_scope");
        let (blocks, update) = config::dispatch(&self.config.data_dir, action);
        if is_default_scope_set
            && let Some(update) = update
            && update.previous != Some(update.current)
        {
            // workspace-level audit event — `project` is None and the
            // artifact_id is the data_dir path (a stable workspace key
            // for V1 until backend-issued workspace UUIDs are wired).
            let workspace_key = self.config.data_dir.display().to_string();
            self.emit_scope_event_local(
                "workspace.scope_default_changed",
                None,
                &workspace_key,
                update.previous,
                update.current,
            );
        }
        blocks
    }
}

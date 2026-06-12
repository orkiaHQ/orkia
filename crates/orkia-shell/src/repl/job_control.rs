// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

impl Repl {
    /// `disown [target]` — remove the job from the controller
    /// without killing it. The child keeps running and survives
    /// orkia's exit (own session via setsid at spawn).
    pub(crate) fn handle_disown(&mut self, target: Option<&str>) -> Outcome {
        let jobs = self.jobs.list();
        let id = match target {
            Some(t) => match crate::builtin_resolve::resolve_job_target(t, &jobs) {
                Some(id) => id,
                None => return Outcome::Error(format!("disown: no job matching '{t}'")),
            },
            None => match jobs.last() {
                Some(j) => j.id,
                None => return Outcome::Error("disown: no jobs to disown".into()),
            },
        };
        // Also drop project association + injection executor entry
        // — both keyed by job_id so a stale entry would mis-route a
        // later spawn that reuses the id.
        self.injection_executor.unregister(id);
        self.job_projects.write().remove(&id);
        match self.jobs.disown(id) {
            Ok(()) => Outcome::BuiltinOutput {
                blocks: vec![BlockContent::SystemInfo(format!("[{id}] disowned"))],
            },
            Err(e) => Outcome::Error(format!("{e}")),
        }
    }

    /// `wait [target]` — block until the named job completes, or
    /// until all background jobs have finished if no target. Uses
    /// passive polling on `engine.try_wait()`; a future SIGCHLD
    /// fast-path would tighten the latency.
    ///
    /// A live PERSISTENT agent session ends only when killed (agents are
    /// sessions, not function calls), so waiting on one would block the
    /// REPL forever. A named `wait` on such a job is refused with a hint;
    /// the bare `wait` skips them. `--once` dispatches DO terminate after
    /// their single turn and stay waitable.
    pub(crate) async fn handle_wait(&mut self, target: Option<&str>) -> Outcome {
        use tokio::time::{Duration, sleep};

        let resolved_id: Option<orkia_shell_types::JobId> = if let Some(t) = target {
            let jobs = self.jobs.list();
            match crate::builtin_resolve::resolve_job_target(t, &jobs) {
                Some(id) => {
                    if let Some(refusal) = self.local_wait_refusal(id) {
                        return Outcome::Error(refusal);
                    }
                    Some(id)
                }
                // Daemon fallback: a detached job surviving a REPL restart is
                None => match self.resolve_daemon_target(t) {
                    Some(did) => {
                        if let Some(refusal) = self.daemon_wait_refusal(did) {
                            return Outcome::Error(refusal);
                        }
                        return self.wait_daemon_job(did).await;
                    }
                    None => return Outcome::Error(format!("wait: no job matching '{t}'")),
                },
            }
        } else {
            None
        };

        // Poll the controller's reap path until either the named
        // job is gone (resolved_id case) or no jobs remain (`wait`
        // with no arg). 50 ms backoff is fine for human-scale
        // waits — bg jobs typically run for seconds-to-minutes.
        loop {
            let jobs = self.jobs.list();
            let still_alive = match resolved_id {
                Some(id) => jobs.iter().any(|j| j.id == id),
                // Bare `wait`: only jobs that can actually terminate count —
                // a persistent agent session left running must not hang it.
                None => jobs.iter().any(|j| self.local_wait_refusal(j.id).is_none()),
            };
            if !still_alive {
                return Outcome::BuiltinOutput { blocks: Vec::new() };
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    /// Why `wait` must not block on local job `id` — `Some(message)` when the
    /// job is a live persistent agent session (no `--once` recipe), `None`
    /// when it is waitable (shell job, terminal state, or one-shot agent).
    fn local_wait_refusal(&self, id: orkia_shell_types::JobId) -> Option<String> {
        let entry = self.jobs.get(id)?;
        let JobKind::Agent { agent_name, .. } = &entry.kind else {
            return None;
        };
        if matches!(
            entry.state,
            crate::job::JobState::Done { .. } | crate::job::JobState::Failed { .. }
        ) {
            return None;
        }
        if entry.sink_recipe.as_ref().is_some_and(|r| r.once) {
            return None;
        }
        Some(persistent_wait_hint(agent_name, id.0))
    }

    /// Same refusal for a daemon-owned job (pure check in
    /// [`daemon_wait_refusal_for`]).
    fn daemon_wait_refusal(&self, id: u32) -> Option<String> {
        let bridge = self.daemon_jobs.as_ref()?;
        let view = bridge.list().into_iter().find(|v| v.id == id)?;
        daemon_wait_refusal_for(&view)
    }

    pub(crate) async fn handle_attach(&mut self, target: &str) -> Outcome {
        // guesses — only explicit targets are accepted. `fg`/`bg`/`stop`
        // keep the permissive resolver.
        if !attach_target_is_explicit(target) {
            return Outcome::UsageError(
                "usage: attach @name | %n | N:@name (explicit target required)".into(),
            );
        }
        // Same resolver `fg`/`bg`/`stop` use (handles `@name`/`%n`).
        let jobs = self.jobs.list();
        if let Some(id) = resolve_job_target(target, &jobs) {
            if let Some(refusal) = self.native_attach_refusal(id) {
                return refusal;
            }
            if self.jobs.get(id).is_some() {
                return self.run_foreground_job(id).await;
            }
        }
        // Local miss — a flipped/detached agent surviving a REPL restart is
        // terminal to the daemon-held PTY (mirrors the `wait`/`kill` fallback).
        // `%n` addresses job n wherever it lives, so the sigil is dropped for
        // the daemon roster (its resolver takes a plain id).
        if let Some(did) = self.resolve_daemon_target(target.strip_prefix('%').unwrap_or(target)) {
            return self.attach_daemon_job(did).await;
        }
        Outcome::Error(format!(
            "no job matching '{target}' (try `ps` to see live jobs)"
        ))
    }

    pub(crate) async fn handle_fg(&mut self, target: Option<&str>) -> Outcome {
        let jobs = self.jobs.list();
        let id = match target {
            Some(t) => match resolve_job_target(t, &jobs) {
                Some(id) => id,
                // Local miss with an explicit target: fall back to a
                None => match self.resolve_daemon_target(t) {
                    Some(did) => return self.attach_daemon_job(did).await,
                    None => return Outcome::Error(format!("no job matching '{t}'")),
                },
            },
            None => match jobs.first() {
                Some(j) => j.id,
                None => return Outcome::Error("no jobs to foreground".into()),
            },
        };
        if let Some(refusal) = self.native_attach_refusal(id) {
            return refusal;
        }
        if self.jobs.get(id).is_none() {
            return Outcome::Error(format!("job {id} not found"));
        }
        self.run_foreground_job(id).await
    }

    /// `Some(refusal)` when `id` is a live native session — the
    /// Orkia-owned loop has no PTY, so there is nothing to attach to.
    fn native_attach_refusal(&self, id: JobId) -> Option<Outcome> {
        self.jobs.native_inbound(id)?;
        Some(Outcome::Error(format!(
            "[{}] is a native session — it has no terminal. \
             Use `tell {}`, `journal --job {}`, or the final response",
            id.0, id.0, id.0
        )))
    }

    pub(crate) fn handle_bg_target(&mut self, target: Option<&str>) -> Outcome {
        let jobs = self.jobs.list();
        let id = match target {
            Some(t) => match resolve_job_target(t, &jobs) {
                Some(id) => id,
                None => return Outcome::Error(format!("no job matching '{t}'")),
            },
            None => match jobs.first() {
                Some(j) => j.id,
                None => return Outcome::Error("no jobs to background".into()),
            },
        };
        match self.jobs.bg(id) {
            Ok(()) => Outcome::BuiltinOutput {
                blocks: vec![BlockContent::SystemInfo(format!("[{id}] continued"))],
            },
            Err(e) => Outcome::Error(format!("{e}")),
        }
    }

    /// `stop` — orkia-job-only: never falls through to a system kill.
    pub(crate) fn handle_stop_target(&mut self, target: &str) -> Outcome {
        if target.is_empty() {
            return Outcome::UsageError("usage: stop <job_id|agent>".into());
        }
        let jobs = self.jobs.list();
        let Some(id) = resolve_job_target(target, &jobs) else {
            return Outcome::Error(format!("no orkia job matching '{target}'"));
        };
        match self.jobs.stop(id) {
            Ok(()) => Outcome::BuiltinOutput {
                blocks: vec![BlockContent::SystemInfo(format!("[{id}] stopped"))],
            },
            Err(e) => Outcome::Error(format!("{e}")),
        }
    }

    /// `kill` — augmented: orkia job if matched, otherwise system kill.
    pub(crate) fn handle_kill(&mut self, target: &str, signal: Option<&str>) -> Outcome {
        if target.is_empty() {
            return Outcome::UsageError("usage: kill <target> [-SIG]".into());
        }
        let jobs = self.jobs.list();
        match resolve_kill(target, signal, &jobs) {
            KillAction::StopJob(id) => match self.jobs.stop(id) {
                Ok(()) => Outcome::BuiltinOutput {
                    blocks: vec![BlockContent::SystemInfo(format!("[{id}] stopped"))],
                },
                Err(e) => Outcome::Error(format!("{e}")),
            },
            KillAction::SystemKill { target, signal } => {
                // Daemon fallback before treating the target as a raw system
                // pid/name: a detached job surviving a REPL restart is owned by
                if let Some(did) = self.resolve_daemon_target(&target) {
                    return self.kill_daemon_job(did);
                }
                Outcome::BuiltinOutput {
                    blocks: orkia_builtin::kill::system_kill(&target, &signal),
                }
            }
        }
    }

    pub(crate) async fn handle_run(&mut self, cmd: &str, args: &[String]) -> Outcome {
        if cmd.is_empty() {
            return Outcome::UsageError("usage: orkia run <command> [args...]".into());
        }
        let full_cmd = if args.is_empty() {
            cmd.to_string()
        } else {
            format!("{cmd} {}", args.join(" "))
        };
        // `run` is foreground-attached by intent; routes through the
        // brush engine like any other shell line.
        self.dispatch_shell(&full_cmd, false).await
    }

    pub(crate) async fn run_foreground_job(&mut self, id: JobId) -> Outcome {
        let entry = match self.jobs.get_mut(id) {
            Some(e) => e,
            None => return Outcome::Error(format!("job {id} not found")),
        };
        entry.state = crate::job::JobState::Foreground;

        if self.renderer.is_attach_capable() {
            self.run_foreground_attached(id).await
        } else {
            self.run_foreground_yielded(id).await
        }
    }

    /// Widget-mode attach (V4-2). Renderer keeps the alternate screen and
    /// hosts the PTY inside a ratatui widget.
    pub(crate) async fn run_foreground_attached(&mut self, id: JobId) -> Outcome {
        let seal_active = true;
        let handle = match self.jobs.get_mut(id) {
            Some(e) => e.build_attached_handle(seal_active),
            None => return Outcome::Error(format!("job {id} not found")),
        };
        self.renderer.attach_job(handle);
        let outcome = self.renderer.drive_attached();

        match outcome {
            orkia_shell_types::AttachedOutcome::Detached => {
                if let Some(e) = self.jobs.get_mut(id) {
                    e.state = crate::job::JobState::Running;
                }
                let _ = self
                    .jobs
                    .event_tx()
                    .send(crate::job::JobEvent::Detached { id });
                // Renderer already pushed a SystemInfo block; nothing more to emit.
                Outcome::BuiltinOutput { blocks: Vec::new() }
            }
            orkia_shell_types::AttachedOutcome::ChildExited => {
                let code = self
                    .jobs
                    .get(id)
                    .and_then(|e| e.try_exit_code())
                    .unwrap_or(0);
                Outcome::ShellComplete {
                    exit_code: code,
                    output: String::new(),
                }
            }
            orkia_shell_types::AttachedOutcome::Unsupported => {
                // Shouldn't happen — renderer reported it was capable.
                self.run_foreground_yielded(id).await
            }
        }
    }

    /// Fallback: tmux-style yield/reclaim for renderers without widget-mode
    /// attach support (the stdout renderer / `--no-tui`).
    pub(crate) async fn run_foreground_yielded(&mut self, id: JobId) -> Outcome {
        // Validate the job exists before we touch the detector mute
        // gate — avoids muting a phantom job.
        if self.jobs.get(id).is_none() {
            return Outcome::Error(format!("job {id} not found"));
        }
        // Drain any pending DetectorEvent side effects *before*
        // muting the detector. Otherwise a queued `Injected` event
        // (worker already printed its toast, but the actual PTY
        // write happens here in `emit_injection`) gets skipped
        // because attach mode blocks the main loop. Without this,
        // the user sees "prompt injected" but the bytes are never
        // actually sent — the toast lies.
        self.drain_state_machine_events();
        self.drain_plugin_dev_reloads();
        // Mute the prompt detector for this job while the user is
        // looking at it directly. Toasts already printed via
        // ExternalPrinter remain in the user's terminal scrollback
        // (that's fine — they were live signals at the time);
        // mute-then-unmute prevents NEW toasts during attach.
        self.state_machine.on_user_attached(id);
        // One-line banner directly to stderr — visible above the
        // child's first frame so the user knows how to get back.
        // Note: the history replay in `run_foreground` may overwrite
        // this on re-attach depending on the child's first cursor
        // move; that's accepted UX.
        {
            use std::io::Write;
            let mut err = std::io::stderr().lock();
            let jid = orkia_shell_types::render_job_id(JobOwner::Local, id.0, None);
            let _ = writeln!(
                err,
                "  \x1b[90m{jid} attached — \x1b[1mCtrl-Z\x1b[0m\x1b[90m to detach, other keys forward to the child\x1b[0m",
            );
            let _ = err.flush();
        }
        self.renderer.yield_terminal();
        // Suppress live ANSI toast emission everywhere (journal
        // listener + state-machine worker) for the duration of the
        // attach — see the `attach_active` field doc-comment for
        // why this is global rather than per-job.
        self.attach_active
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let result = {
            // Scoped borrow so the &mut JobEntry doesn't outlive the
            // await — we re-borrow below via `get_mut` to update
            // state on detach.
            //
            // The existence check above (`jobs.get(id).is_none()` → early
            // return) makes the lookup here logically infallible, but we
            // still surface a defensive Outcome::Error rather than panic:
            // CLAUDE.md "Never panic in non-test code" applies even to
            // invariant-protected accesses, because a future refactor of
            // the early-check could silently break this assumption.
            let Some(entry) = self.jobs.get_mut(id) else {
                self.attach_active
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                return Outcome::Error(format!(
                    "job {id} disappeared between existence check and attach"
                ));
            };
            foreground::run_foreground(entry).await
        };
        self.attach_active
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.renderer.reclaim_terminal();
        // Unmute and tell the detector to re-evaluate from scratch.
        // If the agent is still blocked (e.g. user detached without
        // resolving the trust prompt), a fresh Attention will fire
        // on the next tick.
        self.state_machine.on_user_detached(id);

        match result {
            Ok(code) if foreground::is_detach(code) => {
                // Bash-strict semantics for shell jobs: Ctrl-Z in
                // foreground sends SIGTSTP to the child's process
                // group (portable-pty already setsid'd the child,
                // so child.pid == pgrp) and marks the job Stopped.
                // For agent jobs we keep "detach = keep running"
                // — agents don't have a meaningful "stopped" state
                // the user can resume with `fg` since their hooks
                // would queue up odd events.
                let kind = self.jobs.get(id).map(|e| e.kind.clone());
                let is_shell = matches!(kind, Some(orkia_shell_types::JobKind::Shell { .. }));
                if is_shell {
                    let label = if let Some(e) = self.jobs.get_mut(id) {
                        // SIGTSTP to the child's pgrp. `signal()`
                        // delivers to the lone child pid, which IS
                        // the pgrp leader thanks to portable-pty's
                        // setsid at spawn. That stops every process
                        // in the group, matching bash Ctrl-Z.
                        let _ = e.signal(libc::SIGTSTP);
                        e.state = crate::job::JobState::Stopped;
                        e.label.clone()
                    } else {
                        String::new()
                    };
                    let _ = self
                        .jobs
                        .event_tx()
                        .send(crate::job::JobEvent::Stopped { id, label });
                } else {
                    if let Some(e) = self.jobs.get_mut(id) {
                        e.state = crate::job::JobState::Running;
                    }
                    let _ = self
                        .jobs
                        .event_tx()
                        .send(crate::job::JobEvent::Detached { id });
                }
                // The event listener in `renderers/shell_mode.rs`
                // prints the line; don't also emit a SystemInfo
                // block here or the user sees it twice.
                Outcome::BuiltinOutput { blocks: Vec::new() }
            }
            Ok(code) => Outcome::ShellComplete {
                exit_code: code,
                output: String::new(),
            },
            Err(e) => Outcome::Error(format!("foreground error: {e}")),
        }
    }

    //
    // Detached agents survive a REPL exit because the daemon owns their runtime,
    // not the local `JobController`. The bare-word `ps`/`wait`/`kill`/`tell`
    // builtins resolve against the local controller first; these helpers let
    // them fall back to the daemon roster so a job surviving a restart is still
    // addressable through the natural REPL surface. All no-ops when no daemon
    // bridge is installed (detached runtime / legacy path) — fail-soft.

    /// Resolve `target` (numeric daemon job id, or agent name) against the
    /// daemon's roster. `None` if no bridge, no match, or the round-trip fails.
    pub(crate) fn resolve_daemon_target(&self, target: &str) -> Option<u32> {
        // Normalize any rendered form to the core the daemon roster matches on:
        // `[N]`/`[N:M]` brackets are stripped, a leading `%` (a caller may pass
        // it pre-stripped or not) and `@name` reduce to the bare id/name.
        let parsed = orkia_shell_types::parse_job_target(target);
        let target = parsed.core.strip_prefix('%').unwrap_or(&parsed.core);
        let target = target.strip_prefix('@').unwrap_or(target);
        let views = self.daemon_jobs.as_ref()?.list();
        if let Ok(id) = target.parse::<u32>()
            && views.iter().any(|v| v.id == id)
        {
            return Some(id);
        }
        // Most-recent LIVE job for that agent name first — a newer done
        // entry (not yet reaped) must not shadow a running session — then
        // most-recent overall (mirrors the local resolver).
        views
            .iter()
            .rev()
            .find(|v| v.agent == target && super::agent_dispatch::daemon_view_is_live(v))
            .or_else(|| views.iter().rev().find(|v| v.agent == target))
            .map(|v| v.id)
    }

    /// job surviving a REPL restart shows up under bare `ps`, not just a
    /// full-path `orkia ps`. Daemon rows skip ids a local job already
    /// occupies (local wins) so `ps` never double-lists. This frontend only
    /// GATHERS — the core renders.
    pub(crate) fn build_ps_model(&mut self) -> orkia_builtin::ps::PsModel {
        let local = self.jobs.list();
        let local_ids: std::collections::HashSet<u32> = local.iter().map(|j| j.id.0).collect();
        let mut rows: Vec<orkia_builtin::ps::PsRow> = local
            .iter()
            .map(orkia_builtin::ps::PsRow::from_job_info)
            .collect();
        if let Some(bridge) = self.daemon_jobs.as_ref() {
            rows.extend(
                bridge
                    .list()
                    .iter()
                    .filter(|v| !local_ids.contains(&v.id))
                    .map(orkia_builtin::ps::PsRow::from_daemon_view),
            );
        }
        orkia_builtin::ps::PsModel {
            agents: Some(
                self.agents
                    .iter()
                    .map(orkia_builtin::ps::PsAgent::from_info)
                    .collect(),
            ),
            jobs: rows,
        }
    }

    /// Block until daemon job `id` reaches a terminal state. The daemon's `wait`
    /// is a server-side blocking poll, so it runs on a blocking task to keep the
    /// REPL's tokio runtime free.
    async fn wait_daemon_job(&self, id: u32) -> Outcome {
        let Some(bridge) = self.daemon_jobs.clone() else {
            return Outcome::Error("wait: daemon unavailable".into());
        };
        let res = tokio::task::spawn_blocking(move || {
            bridge.wait(id, std::time::Duration::from_secs(3600))
        })
        .await;
        match res {
            // exit code (POSIX wait semantics), not its own success — carry
            // it through ShellComplete so `$?` is truthful. Empty output +
            // the no-op default `note_exit` keep this render-silent.
            Ok(Ok((_, code))) => Outcome::ShellComplete {
                exit_code: code,
                output: String::new(),
            },
            Ok(Err(e)) => Outcome::Error(format!("wait: {e}")),
            Err(e) => Outcome::Error(format!("wait: {e}")),
        }
    }

    /// Terminate daemon job `id`.
    fn kill_daemon_job(&self, id: u32) -> Outcome {
        let Some(bridge) = self.daemon_jobs.as_ref() else {
            return Outcome::Error("kill: daemon unavailable".into());
        };
        match bridge.kill(id) {
            Ok(()) => Outcome::BuiltinOutput {
                blocks: vec![BlockContent::SystemInfo(format!(
                    "{} killed",
                    orkia_shell_types::render_job_id(JobOwner::Daemon, id, None)
                ))],
            },
            Err(e) => Outcome::Error(format!("kill: {e}")),
        }
    }

    /// holds the agent's PTY master, so — unlike a local job — there is no fd to
    /// host in a widget; we yield the terminal, enter raw mode, and splice
    /// stdin↔the daemon control socket (the daemon pumps its PTY master back the
    /// other way via `attach::pump`). Ctrl-Z detaches. Mirrors the stdout-
    /// renderer yielded-attach ceremony (`run_foreground_yielded`).
    async fn attach_daemon_job(&mut self, id: u32) -> Outcome {
        let Some(bridge) = self.daemon_jobs.clone() else {
            return Outcome::Error("attach: daemon unavailable".into());
        };
        // Release the terminal from the renderer and suppress live toasts for
        // the duration — exactly like the local yielded attach.
        self.renderer.yield_terminal();
        self.attach_active
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // Raw mode on STDIN so each keystroke reaches the agent verbatim and
        // Ctrl-Z is caught byte-wise by the daemon-attach client. The guard is
        // `!Send` and held with NO `.await` in scope, so driving the blocking
        // splice inline is sound — and forwarding the user's keystrokes IS
        // "blocking on user input", the one I/O the REPL loop may block on (#1).
        let result = match crate::job::raw_termios::RawModeGuard::enter() {
            Ok(_guard) => bridge.attach(id),
            Err(e) => Err(format!("raw mode: {e}")),
        };
        self.attach_active
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.renderer.reclaim_terminal();
        match result {
            Ok(()) => Outcome::BuiltinOutput { blocks: Vec::new() },
            Err(e) => Outcome::Error(format!("attach: {e}")),
        }
    }

    /// Auto-attach after a foreground `@agent` dispatch (interactive REPL,
    /// no `&`): typing `@faye` and hitting Enter should land the user IN the
    /// session, not print a job id. Fires only for a fresh DAEMON spawn —
    /// `JobSpawned` with no local `JobController` entry (a local job keeps
    /// its own foreground machinery; a `tell` to a live agent returns a
    /// different outcome). Fail-soft: any attach error renders as the
    /// outcome, the spawn itself already succeeded.
    pub(super) async fn auto_attach_foreground_spawn(
        &mut self,
        outcome: Outcome,
        background: bool,
    ) -> Outcome {
        if background || !self.interactive {
            return outcome;
        }
        let Outcome::JobSpawned { job_id, .. } = outcome else {
            return outcome;
        };
        if self.jobs.get(job_id).is_some() {
            return outcome;
        }
        self.emit_block(BlockContent::SystemInfo(format!(
            "{} detached — attaching (Ctrl-Z to detach)",
            orkia_shell_types::render_job_id(JobOwner::Daemon, job_id.0, None)
        )));
        self.attach_daemon_job(job_id.0).await
    }

    /// Inject `message` into daemon job `id`'s agent session.
    pub(crate) fn tell_daemon_job(&self, id: u32, message: &str) -> Outcome {
        let Some(bridge) = self.daemon_jobs.as_ref() else {
            return Outcome::Error("tell: daemon unavailable".into());
        };
        match bridge.tell(id, message) {
            Ok(()) => Outcome::BuiltinOutput {
                blocks: vec![BlockContent::SystemInfo(format!(
                    "tell: delivered to job {id}"
                ))],
            },
            Err(e) => Outcome::Error(format!("tell: {e}")),
        }
    }
}

/// Why `wait` must not block on a daemon-owned job — `Some(message)` when the
/// view is a live persistent agent session, `None` when waitable. The view
/// carries the verbatim command line as `label`; `--once` is recognized as a
/// standalone token anywhere in the line (the same rule as the classifier's
/// `split_agent_stage`, the flag's single owner).
pub(super) fn daemon_wait_refusal_for(view: &orkia_shell_types::DaemonJobView) -> Option<String> {
    if !super::agent_dispatch::daemon_view_is_live(view) {
        return None;
    }
    if view.label.split_whitespace().any(|w| w == "--once") {
        return None;
    }
    Some(persistent_wait_hint(&view.agent, view.id))
}

/// The named-`wait`-on-a-persistent-session refusal: actionable hints
/// instead of an indefinite block.
fn persistent_wait_hint(agent: &str, id: u32) -> String {
    format!(
        "wait: '@{agent}' is a persistent agent session; it ends only when killed. \
         Use `tell {agent} <msg>` to message it, `attach @{agent}` to attach, \
         `kill {id}` to end it — or dispatch with `--once` for a one-shot `wait` can block on"
    )
}

/// Explicit attach targets: `@name`, `%n`, `[N]`/`[N:M]` (the rendered daemon
/// forms), and `N:@name` (stage-addressed by name). Bare names and raw
/// pids/job-ids are rejected before any resolution.
fn attach_target_is_explicit(target: &str) -> bool {
    let parsed = orkia_shell_types::parse_job_target(target);
    // `[N]` / `[N:M]` — a bracketed daemon id with a numeric core.
    if parsed.prefer_daemon {
        return parsed.core.parse::<u32>().is_ok();
    }
    let core = parsed.core.as_str();
    if let Some(name) = core.strip_prefix('@') {
        return !name.is_empty();
    }
    if let Some(n) = core.strip_prefix('%') {
        return n.parse::<u32>().is_ok();
    }
    if let Some((job, stage)) = core.split_once(':') {
        return job.parse::<u32>().is_ok()
            && stage.strip_prefix('@').is_some_and(|name| !name.is_empty());
    }
    false
}

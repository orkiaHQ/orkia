// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

impl Repl {
    /// Install a SIGCHLD-driven waker that nudges the
    /// JobController's passive reap whenever any child exits.
    /// Tokio's `signal::unix::signal` keeps us free of an extra
    /// crate dep; we already pull tokio. The wake mechanism is
    /// a clone of the `job_events` sender — handler emits a
    /// synthetic `Continued` (cheap no-op for the REPL drain)
    /// that causes the next loop iteration to call `reap`.
    /// Actual reap detection still happens via the existing
    /// `engine.try_wait()` polling — SIGCHLD just shortens the
    /// latency from "next prompt" to "next signal".
    pub(crate) fn boot_sigchld_waker(&mut self) {
        let job_tx = self.jobs.event_tx().clone();
        tokio::spawn(async move {
            let mut sig = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "sigchld: install failed");
                    return;
                }
            };
            loop {
                // Block until any child changes state.
                if sig.recv().await.is_none() {
                    break;
                }
                // Nudge: send a no-op event that forces the
                // REPL drain to wake up and call `list()` →
                // `reap()` on its next loop iteration. We use
                // an Detached(0) signal since it's the cheapest
                // existing variant; the REPL handler treats
                // unknown ids gracefully.
                let _ = job_tx.send(crate::job::JobEvent::Detached {
                    id: orkia_shell_types::job::JobId(0),
                });
            }
        });
    }

    /// Drain `DetectorEvent`s emitted by per-job detector threads.
    /// Renders attention toasts above the next prompt, performs the
    /// PTY write for ready-prompt body injections, and handles the
    /// `Closed` cleanup signal.
    /// Spawn the state-machine worker thread. Owns the original
    /// `DetectorEvent` receiver from `terminal_state`, formats each
    /// event into a one-line toast, ships it via the renderer's
    /// `ExternalPrinter` worker (instant on-screen print), and
    /// forwards the event itself to the REPL through a *new* mpsc
    /// channel for side-effect handling (journal, write_to_pty,
    /// remove_job). Replaces the synchronous `state_machine_rx`.
    pub(crate) fn boot_state_machine_worker(
        &mut self,
        printer: Option<std::sync::mpsc::Sender<String>>,
    ) {
        let Some(rx) = self.state_machine_rx.take() else {
            return;
        };
        let (repl_tx, repl_rx) = std::sync::mpsc::channel();
        self.state_machine_sideeffect_rx = Some(repl_rx);
        let router = self.event_router.clone();
        let injector = self.injection_executor.clone();
        let state_machine = self.state_machine.clone();
        let attach_active = std::sync::Arc::clone(&self.attach_active);
        let announced_done = std::sync::Arc::clone(&self.announced_done);
        let spawn_result = std::thread::Builder::new()
            .name("orkia-state-worker".into())
            .spawn(move || {
                let mut job_names: std::collections::HashMap<JobId, String> =
                    std::collections::HashMap::new();
                while let Ok(event) = rx.recv() {
                    let label = event_label(&event);
                    tracing::info!(event = label, "state-machine worker: received");
                    learn_job_name(&event, &mut job_names);
                    maybe_print_done_notice(
                        &event, &mut job_names, &attach_active, &announced_done, printer.as_ref(),
                    );
                    maybe_inject(&event, &injector);
                    if maybe_auto_answer_trust(&event, &state_machine, &injector) {
                        continue; // handled — don't toast as "needs approval"
                    }
                    router.on_state_machine(&event, "");
                    route_toast(&event, &attach_active, printer.as_ref());
                    match repl_tx.send(event) {
                        Ok(()) => tracing::info!(
                            event = label,
                            "state-machine worker: forwarded to REPL",
                        ),
                        Err(_) => {
                            tracing::warn!("state-machine worker: forward failed (REPL dropped rx); exiting");
                            break;
                        }
                    }
                }
                tracing::debug!("state-machine worker exiting");
            });
        log_worker_spawn_failure(spawn_result);
    }

    pub(crate) fn emit_attention(&mut self, att: crate::terminal_state::JobAttention) {
        // The state-machine worker already printed the toast live
        // via `format_detector_event` + `ExternalPrinter`. Here we
        // only do the journal side effect.
        let percent = (att.confidence * 100.0).round() as i32;
        let mut env = JournalEnvelope::now(EventType::Lifecycle);
        env.job_id = Some(att.job_id.0);
        env.agent = Some(att.agent_name);
        env.event = Some("attention".into());
        env.message = Some(format!(
            "prompt detected ({percent}%): {:?}",
            att.prompt_type
        ));
        env.source = Some("orkia".into());
        self.emit_journal(env);
    }

    pub(crate) fn emit_injection(
        &mut self,
        job_id: orkia_shell_types::JobId,
        agent_name: &str,
        body: &str,
    ) {
        // PTY bytes are written by the `InjectionExecutor` thread
        // the moment `DetectorEvent::Injected` lands in the
        // state-machine worker — see `boot_state_machine_worker`.
        // The REPL drain only runs `emit_injection` so a journal
        // `Tell` envelope gets emitted (needs `&mut self` for the
        // SEAL chain + store). Toast was printed live by the
        // worker via the external printer.
        let mut env = JournalEnvelope::now(EventType::Tell);
        env.job_id = Some(job_id.0);
        env.agent = Some(agent_name.to_string());
        env.event = Some("prompt_injected".into());
        env.message = Some(body.to_string());
        env.source = Some("orkia".into());
        self.emit_journal(env);
    }

    pub(crate) fn emit_prompt_dropped(&mut self, dropped: &crate::terminal_state::DroppedPrompt) {
        let tag = format!(
            "\x1b[90m[job {} {}]\x1b[0m",
            dropped.job_id.0, dropped.agent_name
        );
        let count = dropped.bodies.len();
        let preview = dropped
            .bodies
            .first()
            .map(|b| truncate(b, 60))
            .unwrap_or_default();
        let line = if count > 1 {
            format!(
                "  {tag} \x1b[31m⚠ {count} pending prompts dropped:\x1b[0m \"{preview}\", … (state: {:?})",
                dropped.state_at_drop
            )
        } else {
            format!(
                "  {tag} \x1b[31m⚠ pending prompt dropped:\x1b[0m \"{preview}\" (state: {:?})",
                dropped.state_at_drop
            )
        };
        // Drop is also live-printed by the worker; keep an entry in
        // `notification_queue` so it ALSO appears above the next
        // prompt — drops are a permanent signal the user should not
        // miss even if they missed the live toast.
        self.notification_queue.push(line);

        // One journal entry per dropped body so external queries can
        // see what specific bodies were abandoned.
        for body in &dropped.bodies {
            let mut env = JournalEnvelope::now(EventType::Lifecycle);
            env.job_id = Some(dropped.job_id.0);
            env.agent = Some(dropped.agent_name.clone());
            env.event = Some("prompt_dropped".into());
            env.message = Some(body.clone());
            env.source = Some("orkia".into());
            self.emit_journal(env);
        }
    }
}

// ── State-machine worker helpers ─────────────────────────────────────────

/// Log a state-machine worker spawn failure gracefully. Spawning can only fail
/// when the process is out of file descriptors or has hit the per-user thread
/// limit; crashing the shell would be a worse outcome than logging and continuing.
fn log_worker_spawn_failure(result: std::io::Result<std::thread::JoinHandle<()>>) {
    if let Err(e) = result {
        tracing::error!(
            ?e,
            "state-machine worker failed to spawn — events from this source will be dropped"
        );
    }
}

fn event_label(event: &crate::terminal_state::DetectorEvent) -> &'static str {
    match event {
        crate::terminal_state::DetectorEvent::Attention(_) => "Attention",
        crate::terminal_state::DetectorEvent::Injected { .. } => "Injected",
        crate::terminal_state::DetectorEvent::Delivered { .. } => "Delivered",
        crate::terminal_state::DetectorEvent::Closed { .. } => "Closed",
    }
}

/// Update `job_names` from events that carry an agent name so the `Closed`
/// handler can surface the job's name in the completion notice.
fn learn_job_name(
    event: &crate::terminal_state::DetectorEvent,
    job_names: &mut std::collections::HashMap<JobId, String>,
) {
    match event {
        crate::terminal_state::DetectorEvent::Attention(att) => {
            job_names.insert(att.job_id, att.agent_name.clone());
        }
        crate::terminal_state::DetectorEvent::Injected {
            job_id, agent_name, ..
        }
        | crate::terminal_state::DetectorEvent::Delivered {
            job_id, agent_name, ..
        } => {
            job_names.insert(*job_id, agent_name.clone());
        }
        _ => {}
    }
}

/// Surface `[N]+ Done` or `[N]+ Exit N` immediately on engine close.
/// Gated on `!attach_active` to avoid scribbling over an attached TUI.
fn maybe_print_done_notice(
    event: &crate::terminal_state::DetectorEvent,
    job_names: &mut std::collections::HashMap<JobId, String>,
    attach_active: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    announced_done: &std::sync::Arc<std::sync::Mutex<std::collections::HashSet<JobId>>>,
    printer: Option<&std::sync::mpsc::Sender<String>>,
) {
    let crate::terminal_state::DetectorEvent::Closed { job_id, exit_code } = event else {
        return;
    };
    let Some(name) = job_names.remove(job_id) else {
        return;
    };
    if attach_active.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let Some(p) = printer else { return };
    let id = job_id.0;
    let line = match exit_code {
        Some(0) => format!("  \x1b[32m[{id}]+ Done\x1b[0m                 {name}"),
        Some(code) => format!("  \x1b[31m[{id}]+ Exit {code}\x1b[0m                 {name}"),
        None => format!("  \x1b[90m[{id}]+ {name} exited\x1b[0m"),
    };
    let _ = p.send(line);
    if exit_code.is_some()
        && let Ok(mut s) = announced_done.lock()
    {
        s.insert(*job_id);
    }
}

/// Fire the PTY byte write for `Injected` events, off the REPL critical path.
fn maybe_inject(
    event: &crate::terminal_state::DetectorEvent,
    injector: &crate::injection_executor::InjectionExecutor,
) {
    if let crate::terminal_state::DetectorEvent::Injected {
        job_id,
        agent_name,
        body,
    } = event
    {
        injector.inject(*job_id, agent_name, body);
    }
}

/// Auto-answer the agent's boot trust modal. Returns `true` when the event
/// was handled so the caller can skip further processing.
fn maybe_auto_answer_trust(
    event: &crate::terminal_state::DetectorEvent,
    state_machine: &crate::terminal_state::TerminalStateMachine,
    injector: &crate::injection_executor::InjectionExecutor,
) -> bool {
    let crate::terminal_state::DetectorEvent::Attention(att) = event else {
        return false;
    };
    use crate::terminal_state::{PendingState, PromptType};
    let answerable = matches!(
        &att.prompt_type,
        PromptType::YesNo | PromptType::MultipleChoice | PromptType::Continuation
    );
    if answerable
        && state_machine.pending_state(att.job_id) == Some(PendingState::WaitingForApproval)
    {
        injector.send_keys(att.job_id, b"\r".to_vec());
        state_machine.on_prompt_resolved(att.job_id);
        tracing::info!(
            job = att.job_id.0,
            "trust: auto-answered agent boot modal (consented dir)",
        );
        return true;
    }
    false
}

/// Print the event toast to the printer (or stderr), skipping if attached.
fn route_toast(
    event: &crate::terminal_state::DetectorEvent,
    attach_active: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    printer: Option<&std::sync::mpsc::Sender<String>>,
) {
    let Some(line) = format_detector_event(event) else {
        return;
    };
    if attach_active.load(std::sync::atomic::Ordering::SeqCst) {
        // User is splicing an attached child's PTY; drop the toast.
        return;
    }
    if let Some(p) = printer {
        let _ = p.send(line);
    } else {
        use std::io::Write;
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "{line}");
        let _ = err.flush();
    }
}

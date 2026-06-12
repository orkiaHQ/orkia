// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Interactive PTY spawn + detector-gated injection for one pipeline
//! stage.
//!
//! CLAUDE.md non-negotiable: an agent only ever runs as a full
//! interactive TUI session driven over a PTY — never in print/headless
//! mode. Pipeline stages therefore spawn through the *same* primitives
//! the REPL's Solo dispatch uses:
//!
//!   * [`orkia_terminal_core::TerminalEngine`] — allocates the PTY and
//!     runs the reader/extractor/render engine.
//!   * [`orkia_shell::terminal_state::TerminalStateMachine`] — the
//!     prompt detector; decides when the agent's input box is ready.
//!   * [`orkia_shell::injection_executor::InjectionExecutor`] — types
//!     the composed body into the PTY byte-by-byte and confirms it
//!     landed before submitting.
//!
//! It is *one* execution model, reused — not a second one. This module
//! is provider- and pipeline-agnostic: it spawns whatever `command` it
//! is handed and types whatever bytes it is given. All resolution and
//! orchestration decisions are made upstream (the kernel brain).

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use orkia_shell::injection_executor::InjectionExecutor;
use orkia_shell::terminal_state::{DetectorEvent, PendingState, PromptType, TerminalStateMachine};
use orkia_shell_types::job::JobId;
use orkia_terminal_core::{EngineConfig, TerminalEngine};

/// Everything one stage needs to spawn its agent interactively. Bundled
/// to keep [`StageAgentDriver::spawn_stage`] within the 4-argument limit.
pub(crate) struct StageSpawn<'a> {
    /// The provider program to exec (already resolved by the kernel).
    pub(crate) command: &'a str,
    /// Argv tail for `command`.
    pub(crate) args: &'a [String],
    /// The instruction + carry bytes to deliver to the agent's input box.
    pub(crate) composed: &'a [u8],
    /// The stage run directory — becomes the agent's cwd.
    pub(crate) run_dir: &'a Path,
    /// Env the agent inherits (MCP config path, pipeline/stage ids, …).
    pub(crate) env: Vec<(String, String)>,
    /// Synthetic per-stage job id used to key the detector + typist.
    pub(crate) job_id: u32,
    pub(crate) agent: &'a str,
}

/// Owns the interactive-injection machinery shared across every stage:
/// one PTY-byte typist and one prompt detector, plus a worker thread
/// bridging detector "ready" decisions to the typist. Cheap to clone —
/// both handles wrap channel senders.
#[derive(Clone)]
pub(crate) struct StageAgentDriver {
    injector: InjectionExecutor,
    state_machine: TerminalStateMachine,
}

impl StageAgentDriver {
    /// Stand up the typist, the detector, and the bridging worker. The
    /// worker is a process-lifetime daemon thread (matches the REPL's
    /// `orkia-state-worker`): it lives as long as the executor.
    pub(crate) fn new() -> Self {
        let injector = InjectionExecutor::spawn();
        let state_machine = TerminalStateMachine::new();
        if let Some(rx) = state_machine.take_event_rx() {
            let injector_worker = injector.clone();
            let sm_worker = state_machine.clone();
            let spawned = std::thread::Builder::new()
                .name("orkia-pipe-injector".into())
                .spawn(move || run_worker(rx, injector_worker, sm_worker));
            if let Err(e) = spawned {
                // Fail-closed visibility: without the worker, stage bodies
                // never get typed. Log loudly; the stage timeout will
                // surface the stall rather than hanging silently.
                tracing::error!(
                    ?e,
                    "pipeline: injection worker spawn failed; stage bodies will not be delivered",
                );
            }
        }
        Self {
            injector,
            state_machine,
        }
    }

    /// Spawn `spawn.command` over a real interactive PTY, wire the
    /// detector + typist so the composed body is delivered once the
    /// agent's input prompt is ready, and return the live engine. The
    /// caller owns the engine and must call [`Self::teardown`] when the
    /// stage ends.
    pub(crate) fn spawn_stage(&self, spawn: StageSpawn<'_>) -> Result<TerminalEngine, String> {
        let cfg = EngineConfig {
            cmd: Some(spawn.command.to_string()),
            args: spawn.args.to_vec(),
            env: spawn.env,
            cwd: Some(PathBuf::from(spawn.run_dir)),
            // Agent providers host a persistent full-screen TUI that never
            // emits OSC-133 command markers; keep the grid live so the
            // detector can read the input box and confirm typed bytes.
            persistent_program: true,
            ..EngineConfig::default()
        };
        let engine = TerminalEngine::start(cfg).map_err(|e| e.to_string())?;
        let jid = JobId(spawn.job_id);
        // Register the typist (PTY writer + grid probe) BEFORE the
        // detector, so a fast "ready" decision always finds a writer.
        self.injector
            .register(jid, engine.writer(), Some(engine.grid_probe()));
        match engine.child_id() {
            Some(pid) => {
                let body = (!spawn.composed.is_empty())
                    .then(|| String::from_utf8_lossy(spawn.composed).into_owned());
                self.state_machine
                    .register_agent_job(jid, spawn.agent, pid, &engine, body);
            }
            None => {
                // No pid → no detector thread → the body can't be
                // delivered interactively. Surface it loudly rather than
                // silently regressing to a non-interactive path.
                self.injector.unregister(jid);
                tracing::error!(
                    job = spawn.job_id,
                    agent = spawn.agent,
                    "pipeline: provider exposed no pid; cannot deliver stage body interactively",
                );
            }
        }
        Ok(engine)
    }

    /// Tear down a stage's engine: stop the typist + detector for this
    /// job and signal the agent to exit. Best-effort throughout — a
    /// stage that already exited just yields harmless errors.
    pub(crate) fn teardown(&self, engine: &TerminalEngine, job_id: u32) {
        let jid = JobId(job_id);
        self.injector.unregister(jid);
        let _ = self.state_machine.remove_job(jid);
        let _ = engine.signal(libc::SIGTERM);
    }
}

/// Bridge detector decisions to the typist. Mirrors
/// `repl::state_machine::{maybe_inject, maybe_auto_answer_trust}` exactly,
/// minus the toast/journal side effects the REPL adds. Runs until the
/// detector channel closes (executor drop / process exit).
fn run_worker(
    rx: mpsc::Receiver<DetectorEvent>,
    injector: InjectionExecutor,
    state_machine: TerminalStateMachine,
) {
    while let Ok(event) = rx.recv() {
        match event {
            // Detector decided the agent is ready: type the queued body.
            DetectorEvent::Injected {
                job_id,
                agent_name,
                body,
            } => injector.inject(job_id, &agent_name, &body),
            // Auto-answer the agent's own boot trust/confirm modal. The
            // stage run dir is created by Orkia, so consenting to its boot
            // dialog is safe — and bounded to the WaitingForApproval boot
            // window, never a post-ready tool-permission prompt.
            DetectorEvent::Attention(att) => {
                let answerable = matches!(
                    att.prompt_type,
                    PromptType::YesNo | PromptType::MultipleChoice | PromptType::Continuation
                );
                if answerable
                    && state_machine.pending_state(att.job_id)
                        == Some(PendingState::WaitingForApproval)
                {
                    injector.send_keys(att.job_id, b"\r".to_vec());
                    state_machine.on_prompt_resolved(att.job_id);
                    tracing::info!(
                        job = att.job_id.0,
                        "pipeline: auto-answered agent boot trust/confirm modal (Orkia run dir)",
                    );
                }
            }
            DetectorEvent::Delivered { .. } | DetectorEvent::Closed { .. } => {}
        }
    }
    tracing::debug!("pipeline: injection worker exiting (detector channel closed)");
}

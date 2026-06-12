// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `JobConfig` and `Attachment` — the descriptor passed to
//! [`super::JobController::spawn`].
//!
//! The job model unifies what used to be `spawn_agent` and
//! `spawn_shell` into a single PTY-backed substrate plus a
//! `Vec<Attachment>` describing which subsystems the controller
//!
//! Attachments deliberately carry data, not behavior. The behavior
//! lives in the matching `JobLifecycleHook` impl in
//! [`super::lifecycle`]; the controller turns each attachment into
//! the right hook during `spawn`.
//!
//! Lives in `orkia-shell` (not `orkia-shell-types`) because
//! `Attachment::AgentContext` carries a full `AgentContext` — a
//! shell-internal type. The leaf enums (`StdinSource`,
//! `ProcessGroupMode`) live in `orkia-shell-types::job_config` to
//! keep the dependency direction clean.

use std::path::PathBuf;

use orkia_shell_types::{ProcessGroupMode, ProviderId, StdinSource};

use crate::agent_context::AgentContext;

/// What the controller spawns. Every field of the resulting PTY-
/// backed child process is described here; nothing about the
/// surrounding subsystems (state machine, SEAL chain, injection
/// executor, OSC 133) lives in this struct — those go in
/// [`Attachment`].
pub struct JobConfig<'a> {
    /// Executable to run (resolved through `PATH` by the child).
    pub command: &'a str,
    /// Vendor identity of the spawned program — the single key every
    /// provider-conditional step (context env/args, hook install)
    /// matches on. Resolved once by the dispatch path via
    /// [`ProviderId::derive`]; plain shell jobs use
    /// [`ProviderId::Generic`].
    pub provider: ProviderId,
    /// Arguments. Caller-owned for the lifetime of the spawn call.
    pub args: &'a [String],
    /// Human-readable tag used in `ps` output and job-control
    /// notifications (`[1] spawned: <label>`).
    pub label: String,
    /// Per-job environment overrides layered on top of the
    /// shell's exported env. Existing keys are replaced.
    pub env: Vec<(String, String)>,
    /// Working directory for the child. `None` inherits the
    /// shell's current cwd.
    pub working_dir: Option<PathBuf>,
    /// Stdin wiring — see [`StdinSource`].
    pub stdin: StdinSource,
    /// Process group / session disposition — see
    /// [`ProcessGroupMode`].
    pub process_group: ProcessGroupMode,
    /// Composable wiring requests. Order is significant: hooks
    /// fire in insertion order so callers that depend on event
    /// ordering (SEAL chain genesis before any downstream event,
    /// for example) place attachments accordingly.
    pub attachments: Vec<Attachment>,
    /// spawn `command` directly (default, unchanged behaviour).
    /// `Some` → the controller exec's
    /// `<cage_bin> --policy <policy_path> -- <command> <args…>`
    /// instead. The job's identity (kind, label, agent attribution)
    /// is unchanged — only the exec'd process is wrapped.
    pub cage_wrapper: Option<CageWrapper>,
}

/// Describes how to wrap a job's argv in the Orkia Cage launcher.
/// Carried on [`JobConfig`]; consumed by [`super::spawn`] when it
/// builds the engine command. Shell-internal (lives here, not in
/// `orkia-shell-types`).
#[derive(Debug, Clone)]
pub struct CageWrapper {
    /// Launcher binary (resolved via `$PATH` by the child), e.g.
    /// `orkia-cage`.
    pub cage_bin: String,
    /// Policy file path passed as `--policy <path>`.
    pub policy_path: String,
}

/// Per-job wiring requests. Each variant tells the controller to
/// install a specific [`super::lifecycle::JobLifecycleHook`].
///
/// All attachments are optional — a bare shell job carries an
/// empty `Vec`; a full agent job carries the canonical agent
/// preset assembled by the REPL's agent-dispatch path.
pub enum Attachment {
    /// Install a project-scoped hook config (`<provider>/settings.json`
    /// or equivalent) into the current working directory so the
    /// agent CLI forwards its hook events to `orkia bridge`. The
    /// provider identity comes from [`JobConfig::provider`];
    /// `ProviderId::Generic` is a no-op. When `mediate` is set (a caged
    /// spawn), the Claude config also gets the `orkia-sh hook` PreToolUse
    /// and the config is unchanged from before.
    Hooks { mediate: bool },
    /// Register the job with the prompt detector. Spawns a per-job
    /// detector thread that watches the PTY for ready prompts and
    /// surfaces `Attention` / `Injected` / `Closed` events.
    ///
    /// `pending_body` is queued for injection once the agent's
    /// ready prompt fires. Use `None` when the intent has already
    /// been written via [`StdinSource::InitialBytes`] (hook-driven
    /// agent path) or when there's no intent (shell job).
    StateMachine { pending_body: Option<String> },
    /// Reserved for the pending-prompt FIFO. The `StateMachine`
    /// attachment already manages the queue today; this variant
    /// exists for forward compatibility (a future shell job that
    /// wants `tell`-style follow-ups without the prompt detector
    /// would carry `PendingPrompt` alone).
    PendingPrompt,
    /// Register the job's PTY writer with the injection executor.
    /// Detector-driven `Injected` events then write bytes from
    /// the executor thread without needing a REPL drain.
    InjectionExecutor,
    /// Emit `agent.spawn` (and on completion `agent.complete`)
    /// custom events so the SEAL consumer can create the per-job
    /// chain at `agents/<name>/jobs/<id>/`. When `project` is
    /// `Some`, the consumer also writes a `job.reference` record
    /// into the project chain on completion.
    SealChain { project: Option<String> },
    /// Write the assembled `AgentContext` (system prompt, memory,
    /// MCP config) into the per-job run dir and inject the
    /// matching `ORKIA_AGENT_*` env vars. The hashes returned by
    /// the spawn call come from this attachment.
    AgentContext { context: AgentContext },
    /// Wire OSC 133 + APC callbacks on the underlying
    /// `TerminalEngine` so prompt-block markers and Orkia
    /// protocol payloads route through the unified `EventRouter`.
    Osc133Listener,
}

impl JobConfig<'_> {
    /// True when any `AgentContext` attachment is present —
    /// drives the choice between `JobKind::Agent` and
    /// `JobKind::Shell` when the controller builds the entry.
    pub fn has_agent_context(&self) -> bool {
        self.attachments
            .iter()
            .any(|a| matches!(a, Attachment::AgentContext { .. }))
    }
}

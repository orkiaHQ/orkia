// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Extension traits and event types surfaced by in-process services
//! that the shell can be configured with. Implementations live in
//! sibling crates (e.g. `orkia-final-response`); this crate owns only
//! the data shape and the trait so consumers can depend on a thin,
//! permissive surface without pulling in the implementation.
//!

use std::path::PathBuf;
use std::sync::Arc;

use crate::JournalEnvelope;

/// One emission of an agent's final response for a turn. Built by the
/// `orkia-final-response` service when a `Stop` envelope is observed,
/// the provider's transcript is read, and the assistant's final text is
/// extracted. Mirrors the `AgentFinalResponse` journal envelope but
/// carries typed fields for in-process subscribers (the journal carries
/// the same data as optional `response_*` fields on `JournalEnvelope`).
#[derive(Clone, Debug)]
pub struct FinalResponseEvent {
    pub job_id: u32,
    pub agent: String,
    pub session_id: Option<String>,
    /// Absolute path to `final-response.md` on disk. `None` when
    /// extraction failed; the preview field carries the reason.
    pub response_path: Option<PathBuf>,
    /// First 16 hex characters of SHA-256(file content). `None` on
    /// extraction failure.
    pub response_sha256: Option<String>,
    /// Bytes on disk (post-truncation if applicable). 0 on failure or
    /// tool-only turns.
    pub response_bytes: u64,
    /// First 280 chars of the response, or a `<…>`-bracketed marker on
    /// failure / empty-turn. Always populated.
    pub response_preview: String,
}

/// Callback fired by `FinalResponseSource` on each new event. Runs on
/// the extraction task (not the listener loop) — implementations must
/// not block.
pub type FinalResponseCallback = Arc<dyn Fn(FinalResponseEvent) + Send + Sync>;

/// Read-only handle to the final-response service. Lets in-process
/// consumers (the REPL, future `orkia recall`, Team pipeline
/// coordinator) subscribe to and query final responses without
/// depending on the implementation crate. Implementations live in
/// `orkia-final-response`.
pub trait FinalResponseSource: Send + Sync {
    /// Register a callback invoked every time an `AgentFinalResponse`
    /// event is produced. Callbacks fire on the extraction task, not on
    /// the listener loop — implementations must not block.
    fn subscribe(&self, callback: FinalResponseCallback);

    /// Return the most recent `FinalResponseEvent` recorded for a given
    /// job, or `None` if no `Stop` has been processed yet for it.
    fn latest_for_job(&self, job_id: u32) -> Option<FinalResponseEvent>;

    /// Feed one streamed `AgentFinalResponse` envelope into a passive
    /// source (daemon-hosted-hub mode, where the authoritative extraction
    /// runs daemon-side and the REPL only sees the resulting envelopes).
    /// Active sources that produce their own events from extraction (the
    /// in-process `FinalResponseService`) ignore this — default no-op.
    fn ingest_streamed(&self, _env: &JournalEnvelope) {}
}

/// Listener-side hook that observes every `Stop` envelope after it has
/// been parsed and forwarded. Implementations (notably
/// `orkia-final-response::FinalResponseService`) decide whether to act
/// on it and must not block — work belongs on a spawned task.
///
/// Defined here, not in the implementation crate, so the journal
/// listener in `orkia-shell` can hold an `Arc<dyn JournalStopHook>`
/// without depending on the implementation.
pub trait JournalStopHook: Send + Sync {
    fn on_stop(&self, env: &JournalEnvelope);
}

/// Listener-side hook for every envelope (not just Stop). The Team
/// pipeline coordinator subscribes here to consume `PipelineOutput`
/// events emitted by the MCP pipe server. Defined in shell-types so
/// the journal listener can hold an `Arc<dyn JournalEnvelopeHook>`
/// without depending on Team crates.
///
/// Implementations must not block — work belongs on a spawned task or
/// a non-blocking channel send.
pub trait JournalEnvelopeHook: Send + Sync {
    fn on_envelope(&self, env: &JournalEnvelope);
}

/// Observes each [`crate::job::JobEvent`] the REPL drains. Defined in
/// shell-types so the REPL can hold an `Arc<dyn JobEventObserver>` without
/// depending on the bin's daemon-client code.
///
/// Installed ONLY by a detached agent runtime (MIGRATE-AGENT-SPAWN-TO-DAEMON
/// and forwards each up to the daemon, which relays them to the main REPL as a
/// `StreamFrame::JobEvent` so the REPL learns a daemon-owned job's transitions
/// without polling. The main REPL never installs one.
///
/// Implementations must not block — the REPL drain calls this on the hot loop.
pub trait JobEventObserver: Send + Sync {
    fn on_job_event(&self, event: &crate::job::JobEvent);

    /// Block until every event already handed to `on_job_event` has been
    /// delivered to its sink, or `timeout` elapses. Default: no-op — a
    /// synchronous observer buffers nothing. The detached-runtime forwarder
    /// does the socket send on a background thread, so it overrides this and
    /// the runtime calls it on its teardown exit path: without flushing, the
    /// process can exit mid-send and a terminal `Completed` never reaches the
    /// daemon (the main REPL would never render `[1] done`).
    fn flush_pending(&self, _timeout: std::time::Duration) {}
}

/// One daemon-owned job as seen by the main REPL's builtins
///
/// Detached agents survive REPL exit because the `pty_daemon` owns
/// their runtime, not the in-process `JobController`. Without this view
/// the bare-word `ps` / `wait` / `kill` / `tell` builtins only see
/// REPL-local jobs and report "no job matching '1'" for a job the
/// daemon is happily running. The trait lets the main REPL fold the
/// daemon's roster into those builtins. Fields are already-resolved
/// scalars so the shell crate never sees the binary-local protocol
/// types.
#[derive(Clone, Debug)]
pub struct DaemonJobView {
    pub id: u32,
    pub agent: String,
    pub state: String,
    pub pid: Option<u32>,
    pub label: String,
    pub runtime_secs: u64,
    /// The daemon's recorded exit code, when the job has finished
    /// not a hardcoded 0).
    pub exit_code: Option<i32>,
    /// Pipeline sub-agent stages, so the REPL `ps` can render them
    pub stages: Vec<DaemonStageView>,
}

/// One pipeline stage of a daemon-owned job, as seen by the main REPL
/// (mirror of the binary-local `DaemonStageInfo` protocol type).
#[derive(Clone, Debug)]
pub struct DaemonStageView {
    pub id: u32,
    pub target: String,
    pub state: String,
    pub pid: Option<u32>,
    pub runtime_secs: u64,
    pub exit_code: Option<i32>,
    pub attachable: bool,
}

/// Read-only + control bridge into the `pty_daemon` for the main REPL's
/// job builtins. Implemented in the binary over `client_api` (the shell
/// crate can't see the daemon protocol types), injected via
/// `Repl::with_daemon_jobs`. ONLY the main REPL installs one — a
/// detached runtime leaves it `None` so it never recurses into its own
/// daemon.
///
/// Methods talk to the daemon over its control socket. They run on the
/// REPL thread only inside an explicit builtin dispatch (never the hot
/// drain loop), so a bounded blocking round-trip is acceptable here.
pub trait DaemonJobs: Send + Sync {
    /// Every job the daemon currently owns. Empty on socket error
    /// (fail-soft: a dead daemon must not break `ps`).
    fn list(&self) -> Vec<DaemonJobView>;

    /// Block until job `id` reaches a terminal state or `timeout`
    /// elapses. `Ok((rendered_line, exit_code))` on resolution — the
    /// job's recorded exit code (0 when the daemon recorded none) so
    /// `Err(message)` if no such job or the round-trip fails.
    fn wait(&self, id: u32, timeout: std::time::Duration) -> Result<(String, i32), String>;

    /// Terminate job `id`. `Err` if no such job or the request fails.
    fn kill(&self, id: u32) -> Result<(), String>;

    /// Inject `message` into job `id`'s agent session (the `tell`
    /// builtin). `Err` if no such job or the request fails.
    fn tell(&self, id: u32, message: &str) -> Result<(), String>;

    /// Splice the caller's terminal to daemon job `id`'s live PTY (the
    /// `attach`/`fg` builtins, daemon fallback). The daemon owns the agent's
    /// PTY master — this drives a raw stdin↔socket byte splice and BLOCKS until
    /// the user detaches (Ctrl-Z) or the job exits. The CALLER must place the
    /// terminal in raw mode before and restore it after — this method does not
    /// touch termios so the REPL keeps single ownership of terminal state.
    /// `Err` if no such job or the round-trip fails.
    fn attach(&self, id: u32) -> Result<(), String>;
}

/// A request to spawn an agent in the `pty_daemon` instead of in-process
/// binary's `client_api::SpawnDetachedRequest`, kept deliberately minimal:
/// the transport model re-sends the ORIGINAL command line and the detached
/// runtime re-derives agent context, cage wrapper, hooks, and stdin handling
/// from its own config (exactly as `orkia --detach -c '@agent …'` already
/// does). So the only fields are what the runtime cannot re-derive:
///
/// - `command` — the raw, unparsed REPL command line (`@faye review src/x.rs`,
///   `cat f | @faye`, `@faye execute rfc …`). Must satisfy `needs_repl_pipeline`.
///   The runtime re-parses it through the identical classifier → dispatch.
/// - `working_dir` — the REPL's agent cwd, so the runtime (and the agent) run
///   in the user's directory, not the daemon's.
/// - `agent_name` — for the daemon's job roster/label.
/// - `extra_env` — only values the command line + config can't reconstruct
///   (e.g. the RFC project for `record_reasoning_scope` attribution).
/// - `cage_wrapper` — the cage launcher the REPL resolved for this agent. The
///   detached runtime cannot reliably re-derive it (it does not always load the
///   REPL's `[cage]` config), so the REPL's decision travels on the request: a
///   daemon-owned agent is caged iff the REPL would have caged it. `None` →
///   spawn uncaged.
#[derive(Clone, Debug, Default)]
pub struct DetachedSpawnRequest {
    pub command: String,
    pub working_dir: Option<String>,
    pub agent_name: Option<String>,
    pub extra_env: Vec<(String, String)>,
    pub cage_wrapper: Option<DetachedCageWrapper>,
}

/// The cage launcher + policy for a detached spawn — the transport mirror of
/// the shell crate's `job::CageWrapper` (which this crate cannot name). Mapped
/// to the daemon's wire `CageWrapperProto` by the spawner bridge.
#[derive(Clone, Debug, Default)]
pub struct DetachedCageWrapper {
    pub cage_bin: String,
    pub policy_path: String,
}

impl DetachedSpawnRequest {
    /// Start from the raw command line; set the optional fields as needed.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            ..Default::default()
        }
    }
}

/// Spawn an agent in the `pty_daemon` so it survives REPL exit
/// `client_api` (the shell crate can't see the daemon protocol types), injected
/// via `Repl::with_detached_spawner`. ONLY the main REPL installs one — a
/// detached runtime leaves it `None` (the `ORKIA_DETACHED_JOB_ID` recursion
/// guard) so it never folds back into its own daemon and instead hosts the
/// agent in-process via `JobController::spawn`. Never spawns in print/headless
/// mode (non-negotiable #5) — the daemon drives a full interactive TUI session.
pub trait DetachedSpawner: Send + Sync {
    /// Spawn `req` in the daemon. Returns the daemon job id (the integer
    /// `orkia ps` / bare `ps` shows) or an error string. The REPL then learns
    /// of the running job via the push stream and the daemon-roster
    /// fold into `ps`/`wait`/`kill`/`tell`.
    fn spawn_detached(&self, req: DetachedSpawnRequest) -> Result<u32, String>;
}

/// A read-only view of identity / plan / capability state for the `whoami`
/// and `plan` builtins. Implemented in `orkia-shell` over the live auth,
/// capability-resolver, and adaptive handles, so this vocabulary crate stays
/// free of `orkia-auth` / `orkia-capabilities` (and their `keyring → turso`
/// transitive deps). The methods return already-rendered lines: all the auth
/// logic and formatting lives behind the trait, on the impl side.
/// the `data_dir` value field on `CommandCtx`.
pub trait AuthView: Send + Sync {
    /// Account, plan, kernel status, and unlocked capabilities (`whoami`).
    fn whoami_lines(&self) -> Vec<String>;
    /// Plan tier and the short capability list (`plan`).
    fn plan_lines(&self) -> Vec<String>;
}

// ── Agent-to-agent pipelines ─────────────────────────────────────────
//
// The parser produces `AgentPipelineRequest` from `@a | @b | …` (plus
// the mixed-form shell prefix when present). The dispatcher routes
// it to the registered `AgentPipelineCoordinator` if any, else
// returns a clear "coordinator required" error. The OSS shell ships
// without a coordinator; consumers can plug one in via the trait.

/// One stage of an agent-to-agent pipeline. Same shape as
/// [`crate::PipelineStage`] — kept distinct so future evolutions of
/// the Team-side stage (cancellation hints, per-stage timeouts) can
/// extend it without disturbing the legacy parser type.
#[derive(Clone, Debug)]
pub struct AgentPipelineStage {
    pub agent: String,
    pub body: String,
}

/// A parsed pipeline ready for dispatch. The two variants reflect the
///
/// - `AgentChain` — `@a | @b | @c`.
/// - `ShellThenAgentChain` — `<shell-pipeline> | @a | @b`. The shell
///   prefix is handled by the existing shell-to-agent input mechanism
///   for the first stage; subsequent stages are pure agent hand-off.
#[derive(Clone, Debug)]
pub enum AgentPipelineRequest {
    AgentChain {
        stages: Vec<AgentPipelineStage>,
    },
    ShellThenAgentChain {
        shell: String,
        stages: Vec<AgentPipelineStage>,
    },
}

impl AgentPipelineRequest {
    /// Stages, regardless of variant.
    pub fn stages(&self) -> &[AgentPipelineStage] {
        match self {
            Self::AgentChain { stages } | Self::ShellThenAgentChain { stages, .. } => stages,
        }
    }

    /// Shell prefix when present.
    pub fn shell_prefix(&self) -> Option<&str> {
        match self {
            Self::AgentChain { .. } => None,
            Self::ShellThenAgentChain { shell, .. } => Some(shell.as_str()),
        }
    }
}

/// In-process event the Team coordinator emits to the REPL on
/// pipeline lifecycle transitions (stage spawned, stage completed,
/// pipeline completed). Solo never produces these events; they exist
/// in the public types crate so the REPL can render them uniformly.
#[derive(Clone, Debug)]
pub enum PipelineProgressEvent {
    Started {
        pipeline_id: String,
        total_stages: u32,
    },
    StageSpawned {
        pipeline_id: String,
        stage_index: u32,
        agent: String,
        job_id: u32,
    },
    StageCompleted {
        pipeline_id: String,
        stage_index: u32,
        agent: String,
        bytes: u64,
        via_mcp: bool,
        elapsed_ms: u128,
    },
    Completed {
        pipeline_id: String,
        elapsed_ms: u128,
    },
    Failed {
        pipeline_id: String,
        stage_index: u32,
        reason: String,
    },
}

/// Outcome of a complete pipeline run, returned by
/// [`AgentPipelineCoordinator::dispatch`]. Maps onto the REPL's
/// existing `Outcome` taxonomy.
#[derive(Clone, Debug)]
pub enum PipelineDispatchOutcome {
    /// Pipeline launched. The coordinator drives the run on its own
    /// task; the REPL has already returned to the prompt by the time
    /// the run completes. Per-stage progress flows through
    /// `PipelineProgressEvent` callbacks (registered via
    /// `subscribe_progress`).
    Launched {
        pipeline_id: String,
        total_stages: u32,
    },
    /// Pipeline rejected before launch (e.g. unknown agent, provider
    /// without MCP support).
    Refused { reason: String },
}

/// Callback fired by the coordinator on each progress event.
pub type PipelineProgressCallback = Arc<dyn Fn(PipelineProgressEvent) + Send + Sync>;

/// Team-only async trait: the multi-agent pipeline coordinator. Solo
/// builds carry `None` and refuse pipelines; Team builds wire an
/// implementation via `Repl::with_pipeline_coordinator(...)`.
///
/// The async machinery uses dyn-compatible boxed futures so the
/// trait can be used as `Arc<dyn AgentPipelineCoordinator>`. Same
/// pattern as the agent provider boundary in
/// `orkia-shell::engine::ShellEngine` traits.
pub trait AgentPipelineCoordinator: Send + Sync {
    /// Validate the request and launch the run. Returns immediately
    /// with `Launched` (the actual execution runs on a coordinator-
    /// owned task) or `Refused` when the request fails pre-flight
    /// validation. Progress is reported via subscribers registered
    /// with `subscribe_progress`.
    fn dispatch<'a>(
        &'a self,
        request: AgentPipelineRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PipelineDispatchOutcome> + Send + 'a>>;

    /// Register a callback fired on every pipeline lifecycle event.
    /// The callback is invoked on the coordinator's executor and must
    /// not block.
    fn subscribe_progress(&self, callback: PipelineProgressCallback);
}

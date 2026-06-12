// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Interactive REPL for the Orkia shell.
//!
//! The `Repl` type and its `impl` are split across the `repl/` submodules
//! (one `impl Repl` block per file) to keep every module under the size limit.
//! All submodules are children of `repl`, so they share `Repl`'s private state
//! directly; cross-module methods are widened to `pub(crate)`.

#[allow(unused_imports)] // central re-export hub: items used selectively across repl/ submodules
pub(crate) mod imports {
    pub(crate) use crate::agent::AgentInfo;
    pub(crate) use crate::approval::{ApprovalWatcher, PendingApproval};
    pub(crate) use crate::attention::AttentionCoordinator;
    pub(crate) use crate::builtin_resolve::{
        KillAction, resolve_job_target, resolve_kill, split_kill_args,
    };
    pub(crate) use crate::classifier::{
        AdaptiveHandle, IntentClassifier, IntentGuess, resolve_mode,
    };
    pub(crate) use crate::config::ShellConfig;
    pub(crate) use crate::decision::{
        BlockContent, CellStyle, Decision, Mode, NoOpReason, Outcome, PipelineStage,
    };
    pub(crate) use crate::engine::BrushSession;
    pub(crate) use crate::error::ShellError;
    pub(crate) use crate::history::History;
    pub(crate) use crate::job::foreground;
    pub(crate) use crate::job::{JobController, JobId, JobOwner, JobState};
    pub(crate) use crate::journal::{EventType, JournalEnvelope, JournalListener, JournalStore};
    pub(crate) use crate::pipeline::parse_pipeline;
    pub(crate) use crate::renderer::{PromptContext, RenderEvent, ShellRenderer, WelcomeInfo};
    pub(crate) use crate::router::AgentRouter;
    pub(crate) use orkia_auth::AuthProvider;
    pub(crate) use orkia_capabilities::CapabilityResolver;
    pub(crate) use orkia_shell_types::ForgeBuilder;
    pub(crate) use orkia_shell_types::Workspace;
    pub(crate) use orkia_shell_types::job::JobKind;
    pub(crate) use orkia_shell_types::{HistoryType, PsFlags};
    pub(crate) use std::path::Path;
    pub(crate) use std::sync::Arc;
    pub(crate) use std::time::Duration;
    pub(crate) use tokio::sync::mpsc;
}
use imports::*;

/// REPL state shared across renderer modes.
///
/// The renderer is boxed and trait-object-erased so the `tui` builtin
/// can swap it at runtime: enter the TUI by replacing the active
/// renderer with a `TuiRenderer`, leave by swapping back to whatever
/// renderer was originally provided. All other state (engine, jobs,
/// SEAL chain, workspace) lives on the REPL itself and is unaffected
/// by the swap.
/// Session-scoped RFC context entered via `rfc cd <id>` and left via
/// `rfc exit`. Holds both the project name (so the right Workspace::project
/// lookup wins) and the RFC id (filesystem slug).
#[derive(Debug, Clone)]
pub(crate) struct RfcScopeState {
    project: String,
    rfc_id: orkia_rfc_core::RfcId,
}

/// Internal tag for `handle_rfc_transition` so the four approval-gated
/// state-machine transitions share one dispatcher.
pub(crate) enum RfcTransitionOp {
    Promote,
    Complete,
    Abandon(String),
    Reopen,
}

impl RfcTransitionOp {
    fn cli_name(&self) -> &'static str {
        match self {
            Self::Promote => "promote",
            Self::Complete => "complete",
            Self::Abandon(_) => "abandon",
            Self::Reopen => "reopen",
        }
    }
}

/// Spawn-time context retained for a `scope=public` agent job so the
/// `job.complete` public emission can be built without re-resolving state
pub(crate) struct PublicJobMeta {
    project: String,
    agent_name: String,
    model: String,
    /// Wall-clock start (RFC3339) — `JobEntry::started_at` is an `Instant`.
    started_at: String,
    /// Monotonic start, for runtime_ms.
    started: std::time::Instant,
}

pub struct Repl {
    renderer: Box<dyn ShellRenderer>,
    // Arc (not Box) so `tick` can hand a clone to `spawn_blocking`: kernel
    // classification does a blocking socket round-trip that must run off the
    // async reactor (BUG-039). `IntentClassifier: Send + Sync + 'static`.
    classifier: Arc<dyn IntentClassifier>,
    router: Box<dyn AgentRouter>,
    /// Shared job→project map for the SEAL consumer. Inserted by
    /// the REPL synchronously at agent spawn (before the
    /// `agent.spawn` event is emitted, so the consumer always
    /// finds the entry); removed when the job's `agent.complete`
    /// closes its chain. The consumer (spawned in `run`) holds
    /// the matching Arc and uses this map to decide which project
    /// chain to write `job.reference` records to.
    job_projects: crate::seal::JobProjects,
    /// `scope=public` projects, kept so the `job.complete` emission can carry
    /// the project/agent/model/start it needs. Inserted at dispatch, removed
    /// when the job completes. Empty for non-public jobs (no public emission).
    public_job_meta: std::collections::HashMap<JobId, PublicJobMeta>,
    history: History,
    config: ShellConfig,
    agents: Vec<AgentInfo>,
    jobs: JobController,
    job_events: mpsc::UnboundedReceiver<crate::job::JobEvent>,
    /// Pushed lifecycle events for daemon-owned jobs (MIGRATE-AGENT-SPAWN-TO-
    /// `StreamFrame::JobEvent` onto a `JobEvent` and sends it here; the REPL
    /// drain de-dups against the jobs it still owns in-process, then re-injects
    /// accepted events onto the in-process channel so the existing drain runs
    /// unchanged. `None` on the local/fallback boot path (no daemon).
    pushed_job_events: Option<mpsc::UnboundedReceiver<crate::job::JobEvent>>,
    approvals: ApprovalWatcher,
    attention: AttentionCoordinator,
    /// Unified event journal. Stores agent hooks, approvals, job
    /// lifecycle, shell SEAL, tells. Stood up in `run()` because the
    /// Unix-socket listener spawns a tokio task and needs an active
    /// runtime. Internal emits route through `journal_tx`.
    journal_store: JournalStore,
    journal_listener: Option<JournalListener>,
    journal_rx: Option<mpsc::UnboundedReceiver<JournalEnvelope>>,
    journal_tx: Option<mpsc::UnboundedSender<JournalEnvelope>>,
    /// binary when it has subscribed to the daemon-hosted hub; consumed by
    /// `boot_journal` to enter subscribed mode (REPL streams envelopes from
    /// the daemon instead of binding `orkia.sock` itself). `None` → legacy
    /// local-hub boot.
    journal_pump_starter: Option<JournalPumpStarter>,
    /// Notification lines queued by `drain_journal_events` for the
    /// next prompt. Each entry is already ANSI-formatted. The renderer
    /// prints them above the prompt and clears the buffer.
    notification_queue: Vec<String>,
    /// Active RFC scope, set by `rfc cd <id>` and cleared by `rfc exit`.
    /// When set, RFC subcommands fall back to this id when no slug is given,
    /// and the prompt renders the rfc scope segment.
    rfc_scope: Option<RfcScopeState>,
    /// Cached rendering of the scope-segment for the current `rfc_scope`.
    /// Computing the segment loads RFC frontmatter and scans the
    /// decisions directory; doing that on every prompt would violate
    /// invariant #1 (audit P2-004). The cache is refreshed eagerly on
    /// `rfc cd` and invalidated by `mark_rfc_scope_dirty`; the prompt
    /// path becomes a pointer read.
    rfc_scope_segment_cache: Option<orkia_shell_types::RfcScopeSegment>,
    /// Cached working directory mirrored from the brush session. The
    /// REPL refreshes this after every dispatched shell command (the
    /// only path that can `cd`); the prompt reads it without locking
    /// `Arc<tokio::sync::Mutex<BrushSession>>`. Replaces an earlier
    /// `brush.lock().await` on the prompt path that could be parked
    /// behind a slow completion worker (audit P2-003).
    cwd_cache: Option<std::path::PathBuf>,
    /// Orkia's record of directories the user has approved (asked once
    /// per directory). Source of truth, projected onto provider configs.
    trust_registry: crate::trust::TrustRegistry,
    /// Maps decision ids to the agent PTY that originally asked. The MCP
    /// this bridge on `orkia_rfc_ask`; the REPL drains it on `rfc resolve`
    /// and writes the answer through `JobController::write_to_pty` so the
    /// agent's stdin reader picks it up.
    rfc_pty_bridge: std::sync::Arc<crate::rfc_state::ClarificationPtyBridge>,
    /// Per-project cache of `RfcStateService` instances. Shared with the
    /// MCP dispatcher so REPL-typed commands and agent calls observe the
    /// same lock state. Background reaper iterates this to expire idle
    rfc_services: std::sync::Arc<crate::rfc_state::RfcServiceCache>,
    /// Ephemeral journal-derived activity model used by MCP KG enrichment.
    /// Owned by a background actor; the REPL and dispatcher only hold a
    /// message-passing handle.
    knowledge_activity: crate::knowledge_activity::KnowledgeActivityHandle,
    workspace: Workspace,
    /// brush-backed shell, lazily initialized in `run()` so `Repl::new()`
    /// stays a non-async constructor. Wrapped in `Arc<Mutex<_>>` so the
    /// rustyline completion worker can share access without contention
    /// (readline and exec are mutually exclusive in the REPL loop).
    ///
    /// INVARIANT (REF-014): at most **two** holders of this lock exist — the
    /// REPL task (`dispatch_shell`/exec) and the completion worker
    /// (`completion::brush_bridge`) — and they are **mutually exclusive in
    /// time**: a completion request is served only while the REPL is parked in
    /// readline, never while a command executes, so there is exactly one
    /// writer at any instant and the lock never contends. The prompt hot path
    /// is already off the lock via `cwd_cache`. Per non-negotiable #2, do NOT
    /// add a third holder: a new consumer must instead take ownership of the
    /// `BrushSession` in a dedicated task and talk to it over a channel (the
    /// `brush_bridge` channel is already half of that path).
    brush: Option<Arc<tokio::sync::Mutex<BrushSession>>>,
    connected: bool,
    should_exit: bool,
    /// True once `run()` (the interactive loop) has started. Gates behavior
    /// that only makes sense with a human at the terminal — e.g. the
    /// auto-attach after a foreground `@agent` spawn. `run_one_command`
    /// (`-c`, cron, detached runtime) never sets it.
    interactive: bool,
    /// Process exit code to terminate with when the REPL loop ends.
    /// Set by `exit N` / `quit N` (bash-style); bare `exit`/`quit`
    exit_status: i32,
    /// `render_outcome` — the single funnel every outcome passes through —
    /// and read to seed brush's `$?` before the next shell command, to
    /// seed `run_one_command`'s exit code, and by bare `exit`/`quit`.
    last_outcome_status: i32,
    /// True if this orkia was invoked as a login shell (`chsh -s`,
    /// `--login`, or argv[0] starts with `-`). Controls whether brush
    /// sources `.profile` vs `.bashrc`. Set by the binary via
    /// [`Self::with_login_shell`].
    login_shell: bool,
    /// Last ~500 blocks, kept so a future `tui` invocation can paint
    /// recent context into the alternate screen on entry.
    recent_blocks: std::collections::VecDeque<crate::decision::BlockContent>,
    /// Factory used by the `tui` builtin to construct a `TuiRenderer`.
    /// orkia-shell does not depend on orkia-shell-tui (which depends on
    /// orkia-shell), so the binary injects this on startup if it wants
    /// the `tui` builtin to work. When `None`, `tui` returns an Error
    /// outcome explaining the renderer wasn't wired in.
    tui_factory: Option<TuiFactory>,
    /// Agents scaffolded by the one-shot legacy migration. Surfaced as a
    /// system-info block the first time `run()` paints the welcome.
    migrated_agents: Vec<String>,
    /// Per-job prompt-detector machinery. Spawns a detector thread on
    /// every agent job, receives `DetectorEvent`s back, surfaces
    /// notifications + auto-injects pending bodies on agent ready.
    state_machine: crate::terminal_state::TerminalStateMachine,
    /// REPL-side receiver for `DetectorEvent`s. Owned briefly until
    /// `boot_state_machine_worker` hands it off to the worker thread.
    state_machine_rx: Option<std::sync::mpsc::Receiver<crate::terminal_state::DetectorEvent>>,
    /// Side-effect-only receiver: the worker thread already printed
    /// the toast; the REPL just journals + does PTY writes for
    /// `Injected` etc. Populated by `boot_state_machine_worker`.
    state_machine_sideeffect_rx:
        Option<std::sync::mpsc::Receiver<crate::terminal_state::DetectorEvent>>,
    /// V1 adapter that lifts hooks / OSC 133 / state-machine events
    /// into a single [`OrkiaEvent`] channel. Cloneable so per-job
    /// detector threads and per-agent BlockParser callbacks all push
    /// into the same sink. Existing consumers stay on their legacy
    /// paths; the unified channel is for the new ones (Surface app,
    /// metrics).
    event_router: crate::protocol::EventRouter,
    /// Held until a downstream consumer takes it. `None` once
    /// `take_orkia_event_rx` has been called.
    orkia_event_rx: Option<tokio::sync::mpsc::UnboundedReceiver<crate::protocol::OrkiaEvent>>,
    /// Dedicated PTY-write thread for detector-driven prompt
    /// injection. Holds the per-job [`SharedWriter`] map so a
    /// `DetectorEvent::Injected` fires bytes to the agent
    /// independently of the REPL drain — fixes the bug where a
    /// queued body sat invisible until the user `attach`ed,
    /// because the REPL was parked in `read_line` and never
    /// drained the side-effect queue.
    injection_executor: crate::injection_executor::InjectionExecutor,
    /// Set to `true` while the REPL is splicing a foreground attach
    /// (any job). Read by background threads (journal listener,
    /// state-machine worker) to suppress live ANSI toast emission —
    /// otherwise their writes land inside the attached child's
    /// alt-screen TUI and scribble over its display. The per-job
    /// detector mute on `state_machine` only covers the attached
    /// job; hooks from *other* agents (e.g. an external claude with
    /// the bridge wired up) still fire and need this global gate.
    attach_active: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Job ids whose `[N]+ Done` notice was already surfaced promptly by
    /// the state-machine worker (off-REPL, via the `ExternalPrinter`)
    /// the instant the agent's engine closed — so `drain_job_events`
    /// suppresses its own duplicate print when it later reaps the job.
    /// Without this, completion notices were deferred until the user's
    /// next command unparked `read_line`.
    announced_done: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<JobId>>>,
    /// Per-job tag of the most recently rendered `JobUpdate` line, used to
    /// suppress consecutive identical lifecycle renders (e.g. a burst of
    /// `[1] detached` relayed up from a detached runtime). REPL-thread-local —
    /// only `drain_job_events` touches it, so no lock is needed.
    last_job_render: std::collections::HashMap<JobId, &'static str>,
    /// Optional final-response service handle. When set, the journal
    /// listener installs it as a `JournalStopHook` so every Stop
    /// envelope spawns a transcript-extraction task; the trait surface
    /// is also exposed to in-process consumers via `final_response_source`.
    final_response_hook: Option<std::sync::Arc<dyn orkia_shell_types::JournalStopHook>>,
    final_response_source: Option<std::sync::Arc<dyn orkia_shell_types::FinalResponseSource>>,
    /// Emit-channel receiver paired with a caller-installed
    /// `final_response_hook` (the Team pipeline bundle's FRS). The local
    /// journal boot relays it into the hub ingress so the hook's
    /// `AgentFinalResponse` envelopes reach the broadcast bus exactly like
    /// the default FRS's. `None` when the REPL builds its own FRS (born
    /// wired to the ingress); harmlessly unused in subscribed mode, where
    /// the daemon-side FRS is the authoritative stop hook.
    final_response_ingress:
        Option<tokio::sync::mpsc::UnboundedReceiver<orkia_shell_types::JournalEnvelope>>,
    /// Concrete handle to the in-process `FinalResponseService`, set only
    /// when this REPL built one (local journal boot). The native runtime
    /// publishes turn outcomes through it (`publish_native`) — a surface
    /// the trait objects above don't expose. `None` in subscribed mode,
    /// where native dispatch flips to the daemon anyway.
    final_response_service: Option<std::sync::Arc<orkia_final_response::FinalResponseService>>,
    /// Optional Team-only multi-agent pipeline coordinator. When set,
    /// `@a | @b [| @c …]` dispatches to it. When `None` (the default
    /// in Solo builds), the dispatcher returns a clear
    pipeline_coordinator: Option<std::sync::Arc<dyn orkia_shell_types::AgentPipelineCoordinator>>,
    /// Optional envelope-level journal hook. The Team coordinator
    /// registers here to observe `PipelineOutput` envelopes emitted
    /// by the MCP pipe server. Solo never registers anything.
    journal_envelope_hook: Option<std::sync::Arc<dyn orkia_shell_types::JournalEnvelopeHook>>,
    /// Optional observer of every drained `JobEvent`. Installed ONLY by a
    /// C) to forward its agent's lifecycle/terminal signals up to the daemon,
    /// which relays them to the main REPL. The main REPL leaves this `None`.
    job_event_observer: Option<std::sync::Arc<dyn orkia_shell_types::JobEventObserver>>,
    /// Optional forwarder for the detached runtime's `AgentFinalResponse`
    /// detached runtime: a bus subscriber in `boot_journal_local` hands each
    /// envelope to it, and the impl forwards just the final-response one up to
    /// the daemon so the main REPL's projection/sink sees the turn. `None` on
    /// the main REPL and the legacy fallback path.
    detached_afr_forwarder: Option<std::sync::Arc<dyn orkia_shell_types::JournalEnvelopeHook>>,
    /// Bridge into the `pty_daemon` so the bare-word `ps` / `wait` /
    /// `kill` / `tell` builtins surface daemon-owned detached jobs
    /// Installed ONLY by the main REPL via `with_daemon_jobs`; a detached
    /// runtime leaves it `None` so it never recurses into its own daemon.
    daemon_jobs: Option<std::sync::Arc<dyn orkia_shell_types::DaemonJobs>>,
    /// Spawns agents through the `pty_daemon` so they survive REPL exit
    /// REPL via `with_detached_spawner`; a detached runtime leaves it `None`
    /// (the `ORKIA_DETACHED_JOB_ID` recursion guard) and hosts agents
    /// in-process via `JobController::spawn`. `Some` ⇒ agent dispatch routes
    /// to the daemon; `None` ⇒ today's in-process behavior, unchanged.
    detached_spawner: Option<std::sync::Arc<dyn orkia_shell_types::DetachedSpawner>>,
    /// capability set. Held as `Option` so legacy call sites that
    /// don't opt into the wiring keep the heuristic baseline.
    capability_resolver: Option<std::sync::Arc<dyn CapabilityResolver>>,
    /// Handle into the [`AdaptiveClassifier`] that lets the auth
    /// builtins swap the kernel RPC in/out without rebuilding the
    /// classifier. `None` whenever `capability_resolver` is also
    /// `None`.
    adaptive_handle: Option<AdaptiveHandle>,
    /// Auth provider used by `$login` / `$logout` / `$whoami` /
    /// `$plan` and by Forge bearer extraction. Injected by the
    /// binary so the shell crate stays backend-agnostic.
    auth_provider: Option<std::sync::Arc<dyn AuthProvider>>,
    /// Local-to-backend publisher. `None` when stream is disabled, not
    stream_handle: Option<orkia_stream::StreamHandle>,
    /// Forge builder. The OSS binary injects `NoopForgeBuilder`; the
    /// proprietary distribution injects a remote `ForgeBuilder`.
    /// Calls are gated on `Capability::ForgeBuild`.
    forge_builder: Option<std::sync::Arc<dyn ForgeBuilder>>,
    /// OSS builds — the assembler's runtime sub-workspace lives outside
    /// the public tree, so the proprietary distribution injects the
    /// concrete implementation via `with_seal_assembler`.
    seal_assembler: Option<std::sync::Arc<dyn orkia_shell_types::RfcSealAssembler>>,
    /// OSS shell defaults to `NoopTeamClient`, which makes every
    /// `$team` / `$invite` / `$members` / `$share` / `$leave`
    /// invocation surface a "Team operations require Orkia Team"
    /// block. Wired in `Repl::new` so the cache always has a backend.
    team_client: std::sync::Arc<dyn orkia_shell_types::TeamClient>,
    /// on stale TTL, workspace switch, mutations, and explicit
    /// `team refresh`.
    team_cache: std::sync::Arc<crate::team_cache::TeamCache>,
    /// Persistent shell session — currently just `current_team`.
    /// Loaded at construction; saved after every mutating builtin
    /// that changes its fields (`team cd`, `team cd --clear`,
    /// `invite accept` once the workspace switches).
    session: crate::session::Session,
    /// Specifically the "scope=team without team membership" warning;
    /// each `(artifact_id, kind)` is surfaced at most once per shell
    /// boot. See `crate::scope_warnings`.
    scope_warnings: std::sync::Arc<crate::scope_warnings::ScopeWarningTracker>,
    /// commands; consulted (via `try_parse_exec`) before the named control/effect
    /// dispatch so typed pipelines route to the streaming engine while POSIX
    /// pipes fall through.
    registry: std::sync::Arc<crate::exec::CommandRegistry>,
    /// recorded at the last stable-command scan. Compared each prompt (cold)
    /// so a binary installed mid-session (which bumps its dir's mtime)
    /// triggers a rescan; unchanged mtimes skip the rescan entirely.
    path_mtimes: Vec<(std::path::PathBuf, Option<std::time::SystemTime>)>,
    /// engine init failed (plugins disabled, shell still runs). Held so
    /// `plugin add` can load a new plugin live.
    plugin_runtime: Option<std::sync::Arc<orkia_plugin::PluginRuntime>>,
    /// threads recompile on save and send the rebuilt command here; the REPL
    /// drains it each tick and swaps it into the registry (dev-mode live
    /// reload). The sender is cloned into each watcher; the channel is created
    /// once and stays empty unless `plugin dev` is used.
    plugin_dev_tx: std::sync::mpsc::Sender<crate::plugins::DevReloadMsg>,
    plugin_dev_rx: std::sync::mpsc::Receiver<crate::plugins::DevReloadMsg>,
    /// Orkia Intelligence handle. Booted in `run()`/`run_one_command()` iff the
    /// login + premium gate is open; stays `None` for anonymous/free users or
    /// when no auth provider is wired. Held so `enrich_active` can inject the
    /// preference block at agent spawn. The reasoning consumer task it owns
    /// reads the journal broadcast bus — the same bus SEAL/stream consume.
    intelligence: Option<orkia_kernel::Intelligence>,
    /// Shared `job_id → project/RFC` map the reasoning consumer reads to stamp
    /// each turn. The REPL is the sole
    /// writer (inserts at agent spawn, removes at reap), mirroring
    /// [`Self::job_projects`]. Always present (cheap); only consulted when
    /// `intelligence` booted.
    reasoning_scopes: orkia_kernel::JobScopes,
    /// True when the dispatched `@agent` carried `--once` (the single canonical
    /// the `-c` path ([`Self::run_one_command`]) from the parsed `once` flag, NOT
    /// from "ran via `-c`": a bare `@faye` dispatched via `-c` (or a detached
    /// runtime) is **persistent** — it idles after its turn and stays addressable
    /// for `tell`/`attach`. Only `--once` (and cron, which generates `--once`)
    /// makes the turn terminal: when the `Stop` hook arrives with no body still
    /// queued, [`Self::stop_oneshot_agent`] kills the session and the run loop
    /// exits. Never set in the interactive REPL ([`Self::run`]).
    oneshot_dispatch: bool,
    /// Set by [`Self::stop_oneshot_agent`] when a one-shot `-c` agent has
    /// delivered its single turn. [`Self::run_one_command`]'s poll loop breaks
    /// on it directly, instead of waiting for the stopped agent's PTY exit to
    /// reap as `Done` — that reap is unreliable (the engine's `try_wait` only
    /// yields the exit code once, so a racing reader can swallow it and the
    /// entry lingers in `Running`/`Stopped` forever). Making turn-completion an
    /// explicit signal is what guarantees `orkia --detach -c "@agent …"`
    /// reaches a terminal daemon state so `orkia wait` returns `done`.
    oneshot_complete: bool,
}

/// Builds a fresh boxed renderer for the TUI sub-loop. Receives the
/// current agent list + workspace so the new TuiRenderer can paint its
/// sidebar immediately.
pub type TuiFactory = Box<
    dyn FnMut(&[AgentInfo], &Workspace) -> Result<Box<dyn ShellRenderer>, ShellError>
        + Send
        + 'static,
>;

/// Handles handed to the bin-provided journal pump in subscribed mode
/// pump"). The bin owns the daemon socket connection and drives the pump
/// threads using these handles:
///
/// - `feed_tx`: daemon-streamed `Envelope` frames are pushed here; the relay
///   fires the REPL-resident live handlers and fans out to the drain + bus.
/// - `emit_rx`: REPL in-process emits (lifecycle, tell, shell SEAL) drain
///   here; the bin forwards each as a `JournalEmit` request to the daemon.
/// - `mcp`: the REPL's real `McpShellDispatcher`. The bin calls it on each
///   `McpProxy` frame and sends the reply back as `McpProxyReply`.
pub struct DaemonJournalHandles {
    pub feed_tx: mpsc::UnboundedSender<JournalEnvelope>,
    pub emit_rx: mpsc::UnboundedReceiver<JournalEnvelope>,
    pub mcp: Arc<dyn crate::journal::McpDispatcher>,
    /// an in-process `JobEvent` and sends it here. The REPL drains this on the
    /// main loop, de-dups against jobs it owns in-process, and re-injects the
    /// rest so the existing lifecycle drain handles them identically.
    pub job_event_tx: mpsc::UnboundedSender<crate::job::JobEvent>,
}

/// Bin-provided closure that starts the daemon journal pump. Invoked once
/// from `boot_journal` when the bin has successfully subscribed to the
/// daemon-hosted hub. `None` (the default) keeps the REPL on the legacy
/// local-hub boot path (fallback when no daemon, older daemon, or a
/// subscribe error).
pub type JournalPumpStarter = Box<dyn FnOnce(DaemonJournalHandles) + Send>;

pub(crate) const RECENT_BLOCKS_CAP: usize = 500;

/// Inputs to [`build_agent_job_config`]. Bundled in a struct so
/// the helper stays under the 4-argument limit (CLAUDE.md).
pub(crate) struct AgentJobConfigInput<'a> {
    agent_name: &'a str,
    cmd: &'a str,
    args: &'a [String],
    extra_env: Vec<(String, String)>,
    agent_context: Option<crate::agent_context::AgentContext>,
    hooks_provider: Option<&'a str>,
    stdin: orkia_shell_types::StdinSource,
    pending_body: Option<String>,
    project: Option<String>,
    /// Directory the agent runs in (the user's shell cwd). Also the
    /// directory whose trust we gate on — so the trusted dir always
    /// matches where the agent actually operates.
    working_dir: Option<std::path::PathBuf>,
    /// [`Repl::cage_wrapper`] from the `[cage]` config; `None` when the
    /// cage is disabled (the default).
    cage_wrapper: Option<crate::job::CageWrapper>,
}

mod agent_dispatch;
mod approval;
mod attention_builtin;
mod builders;
mod builtins_agent;
mod builtins_app;
mod builtins_cap;
mod builtins_misc;
mod builtins_operator;
mod builtins_plugin;
mod builtins_reasoning;
mod builtins_team;
mod builtins_trust;
mod completion;
mod core;
mod dispatch;
// Test seam for the builtin-table exhaustiveness checks.
#[cfg(test)]
pub(crate) use dispatch::{AUTH_SERVICE_ARMS, EFFECTFUL_ARMS, SHELL_CONTROL_ARMS};
mod drains;
mod emission;
mod forge;
mod job_control;
mod journal;
mod native_dispatch;
mod operator_capture;
mod prompt;
mod rfc;
mod rfc_ops;
mod startup;
mod state_machine;
mod streamed_final_response;
pub use streamed_final_response::StreamedFinalResponseSource;
mod util;
pub(crate) use util::*;

#[cfg(test)]
mod job_control_daemon_tests;
#[cfg(test)]
mod journal_oneshot_tests;

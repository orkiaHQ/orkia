// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

impl Repl {
    pub fn new<R: ShellRenderer, C: IntentClassifier, A: AgentRouter>(
        renderer: R,
        classifier: C,
        router: A,
        config: ShellConfig,
    ) -> Self {
        // `NoopTeamClient`. Binaries that wire a real backend swap
        // it in via `with_team_client(...)` before `run()`.
        let team_client: std::sync::Arc<dyn orkia_shell_types::TeamClient> =
            std::sync::Arc::new(orkia_shell_types::NoopTeamClient::new());
        let data_dir_for_session = config.data_dir.clone();
        let session = crate::session::Session::load(&data_dir_for_session);
        let migrated = crate::agent_migration::migrate_legacy_agents(&config);
        let mut config = config;
        if !migrated.is_empty() {
            config.hydrate_agents_from_dir();
        }
        // Migrate the legacy global seal.jsonl if any. Best-effort
        // and idempotent — safe to call on every startup; no-op once
        // already migrated. See `seal::migrate` for the contract.
        let _ = crate::seal::migrate_global_seal(&config.data_dir);
        let job_projects: crate::seal::JobProjects =
            std::sync::Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new()));
        let history = History::new(&config.data_dir);
        let agents = config.agents.clone();
        // A detached agent runtime adopts the daemon's job id for the single
        // agent it hosts, so its `ORKIA_JOB_ID` / storage dir / dispatch
        // `FinalResponseEvent.job_id` stay globally unique across concurrent
        // runtimes (two roots of a dispatch diamond would otherwise both be
        // `1` and cross-wire in the proxy). Main REPL → `1`.
        let next_job_id = crate::detached_control::detached_runtime_job_id().unwrap_or(1);
        let (jobs, job_events) = JobController::with_next_id(next_job_id);
        let approvals = ApprovalWatcher::new(&config.data_dir);
        let attention = AttentionCoordinator::spawn();
        let journal_store = JournalStore::new(&config.data_dir);
        let workspace = Workspace::load(&config.data_dir);
        let state_machine = crate::terminal_state::TerminalStateMachine::new();
        let state_machine_rx = state_machine.take_event_rx();
        // The injection executor reports a confirmed landing by sending
        // `DetectorEvent::Delivered` back onto this same channel, so the
        // worker raises the toast/journal when bytes truly arrive.
        let injection_executor = crate::injection_executor::InjectionExecutor::spawn_with_delivery(
            state_machine.event_sender(),
        );
        let (event_router, orkia_event_rx) = crate::protocol::EventRouter::new_with_rx();
        // if the wasm engine fails to init — plugins disabled, shell unaffected).
        let plugin_runtime = orkia_plugin::PluginRuntime::new()
            .ok()
            .map(std::sync::Arc::new);
        // Hot-reload channel: empty unless `plugin dev` runs.
        let (plugin_dev_tx, plugin_dev_rx) = std::sync::mpsc::channel();
        let config_data_dir = config.data_dir.clone();
        let trust_registry =
            crate::trust::TrustRegistry::load(config_data_dir.join("trusted_dirs.json"));
        Self {
            renderer: Box::new(renderer),
            classifier: Arc::new(classifier),
            router: Box::new(router),
            job_projects,
            public_job_meta: std::collections::HashMap::new(),
            history,
            config,
            agents,
            jobs,
            job_events,
            pushed_job_events: None,
            approvals,
            attention,
            journal_store,
            journal_listener: None,
            journal_rx: None,
            journal_tx: None,
            journal_pump_starter: None,
            notification_queue: Vec::new(),
            rfc_scope: None,
            rfc_scope_segment_cache: None,
            cwd_cache: None,
            trust_registry,
            rfc_pty_bridge: std::sync::Arc::new(crate::rfc_state::ClarificationPtyBridge::new()),
            rfc_services: std::sync::Arc::new(crate::rfc_state::RfcServiceCache::new()),
            knowledge_activity: crate::knowledge_activity::spawn_activity_actor(),
            workspace,
            brush: None,
            connected: false,
            should_exit: false,
            interactive: false,
            exit_status: 0,
            last_outcome_status: 0,
            login_shell: false,
            recent_blocks: std::collections::VecDeque::with_capacity(RECENT_BLOCKS_CAP),
            tui_factory: None,
            migrated_agents: migrated,
            state_machine,
            state_machine_rx,
            state_machine_sideeffect_rx: None,
            event_router,
            orkia_event_rx: Some(orkia_event_rx),
            injection_executor,
            attach_active: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            announced_done: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
            last_job_render: std::collections::HashMap::new(),
            final_response_hook: None,
            final_response_source: None,
            final_response_ingress: None,
            final_response_service: None,
            pipeline_coordinator: None,
            journal_envelope_hook: None,
            job_event_observer: None,
            detached_afr_forwarder: None,
            daemon_jobs: None,
            detached_spawner: None,
            capability_resolver: None,
            adaptive_handle: None,
            auth_provider: None,
            stream_handle: None,
            forge_builder: None,
            dispatch_proxy: None,
            dispatch_runs: Vec::new(),
            seal_assembler: None,
            team_client: team_client.clone(),
            team_cache: std::sync::Arc::new(crate::team_cache::TeamCache::new(
                data_dir_for_session.clone(),
                team_client,
            )),
            session,
            scope_warnings: std::sync::Arc::new(crate::scope_warnings::ScopeWarningTracker::new()),
            oneshot_dispatch: false,
            oneshot_complete: false,
            registry: {
                let mut reg = crate::exec::CommandRegistry::with_pilots();
                if let Some(rt) = plugin_runtime.as_ref() {
                    let dir = crate::plugins::plugin_dir(&config_data_dir);
                    crate::plugins::load_all(&dir, &mut reg, rt);
                }
                std::sync::Arc::new(reg)
            },
            path_mtimes: Vec::new(),
            plugin_runtime,
            plugin_dev_tx,
            plugin_dev_rx,
            intelligence: None,
            reasoning_scopes: orkia_kernel::new_job_scopes(),
        }
    }

    /// The binary calls this after it has connected to the pty-daemon and
    /// completed the `Subscribe` handshake; `boot_journal` then enters
    /// subscribed mode. When never called, the REPL boots its own local hub
    /// (the legacy path / fallback).
    pub fn set_journal_pump_starter(&mut self, starter: crate::repl::JournalPumpStarter) {
        self.journal_pump_starter = Some(starter);
    }

    /// Inject the [`AuthProvider`] that backs `$login` / `$logout`
    /// and supplies bearer tokens for Forge dispatch. The shipped binary
    /// passes `orkia_magic_login::MagicLinkAuthProvider` (real signed-JWT
    /// session, persisted to keychain or `ORKIA_SESSION_FILE`).
    pub fn with_auth_provider(mut self, provider: std::sync::Arc<dyn AuthProvider>) -> Self {
        self.auth_provider = Some(provider);
        self
    }

    /// Inject the [`ForgeBuilder`] that powers `rfc forge` and
    /// `orkia app usage`. Pass `orkia_shell::NoopForgeBuilder` for
    /// OSS builds, or wire the proprietary Forge client for the
    /// proprietary distribution.
    pub fn with_forge_builder(mut self, forge: std::sync::Arc<dyn ForgeBuilder>) -> Self {
        self.forge_builder = Some(forge);
        self
    }

    /// Inject the OSS RFC→many-agents dispatch coordinator that powers
    /// `orkia rfc dispatch`. Left unset in Solo builds (the command then
    /// surfaces a Team-required block). The binary builds it over the
    /// kernel client, the agent resolver, and the detached-spawn seams.
    pub fn with_dispatch_proxy(
        mut self,
        proxy: std::sync::Arc<orkia_dispatch_proxy::KernelDispatchProxy>,
    ) -> Self {
        self.dispatch_proxy = Some(proxy);
        self
    }

    /// OSS builds leave this unset (`rfc complete` skips the SEAL footer
    /// and `orkia rfc seal` reports "not wired"). The proprietary
    /// distribution wires the concrete assembler here.
    pub fn with_seal_assembler(
        mut self,
        assembler: std::sync::Arc<dyn orkia_shell_types::RfcSealAssembler>,
    ) -> Self {
        self.seal_assembler = Some(assembler);
        self
    }

    /// OSS shell defaults to `NoopTeamClient`; the proprietary
    /// distribution wires a GraphQL-backed implementation here.
    /// Rebuilds the `TeamCache` so the new client is the one that
    /// runs `bootstrap()` from now on.
    pub fn with_team_client(
        mut self,
        client: std::sync::Arc<dyn orkia_shell_types::TeamClient>,
    ) -> Self {
        self.team_client = client.clone();
        self.team_cache = std::sync::Arc::new(crate::team_cache::TeamCache::new(
            self.config.data_dir.clone(),
            client,
        ));
        self
    }

    /// wiring. The `$login`/`$logout`/`$whoami`/`$plan` builtins use
    /// these to read the active plan and swap the kernel handle in
    /// and out of the [`AdaptiveClassifier`] without restarting.
    ///
    /// Callers that don't wire this keep the original heuristic-only
    /// behaviour — the builtins still parse, but they degrade to
    /// "no resolver / no kernel" output.
    pub fn with_capabilities(
        mut self,
        resolver: std::sync::Arc<dyn CapabilityResolver>,
        adaptive_handle: AdaptiveHandle,
    ) -> Self {
        self.capability_resolver = Some(resolver);
        self.adaptive_handle = Some(adaptive_handle);
        self
    }

    /// Wire the journal listener to dispatch every Stop envelope to the
    /// given hook (typically an `Arc<FinalResponseService>` from the
    /// `orkia-final-response` crate). The same value usually implements
    /// [`orkia_shell_types::FinalResponseSource`] — pass it via
    /// [`Self::with_final_response_source`] to expose the read-only
    /// surface to other in-process consumers (recall, future Team).
    pub fn with_final_response_hook(
        mut self,
        hook: std::sync::Arc<dyn orkia_shell_types::JournalStopHook>,
    ) -> Self {
        self.final_response_hook = Some(hook);
        self
    }

    pub fn with_final_response_source(
        mut self,
        source: std::sync::Arc<dyn orkia_shell_types::FinalResponseSource>,
    ) -> Self {
        self.final_response_source = Some(source);
        self
    }

    /// Hand over the emit-channel receiver paired with a caller-installed
    /// `final_response_hook`. The local journal boot relays it into the hub
    /// ingress so the hook's `AgentFinalResponse` envelopes reach the
    /// broadcast bus (disk tee, detached AFR forwarder) exactly like the
    /// default FRS's. Without this, a pre-installed hook's emits drain
    /// nowhere — a Team-authed detached runtime then never forwards its
    /// turn up to the daemon (no journal row, no projection, no sink).
    pub fn with_final_response_ingress(
        mut self,
        rx: tokio::sync::mpsc::UnboundedReceiver<orkia_shell_types::JournalEnvelope>,
    ) -> Self {
        self.final_response_ingress = Some(rx);
        self
    }

    /// Expose the trait surface to in-process consumers (Solo recall,
    /// Team pipeline coordinator). Returns `None` if the binary did not
    /// wire a `FinalResponseService`.
    pub fn final_response_source(
        &self,
    ) -> Option<&std::sync::Arc<dyn orkia_shell_types::FinalResponseSource>> {
        self.final_response_source.as_ref()
    }

    /// Wire a multi-agent pipeline coordinator. When set, `@a | @b`
    /// dispatches to it; otherwise the dispatcher returns a clear
    /// "pipeline coordinator required" error. The OSS shell ships
    /// without a coordinator; downstream integrations (including
    /// Orkia OS) can provide one by implementing
    /// `AgentPipelineCoordinator`.
    pub fn with_pipeline_coordinator(
        mut self,
        coord: std::sync::Arc<dyn orkia_shell_types::AgentPipelineCoordinator>,
    ) -> Self {
        self.pipeline_coordinator = Some(coord);
        self
    }

    /// Wire an envelope-level journal hook. The Team coordinator
    /// registers here so it can observe `PipelineOutput` envelopes
    /// emitted by the MCP pipe server. Multiple hooks may eventually
    /// be supported; v1 keeps a single hook per build.
    pub fn with_journal_envelope_hook(
        mut self,
        hook: std::sync::Arc<dyn orkia_shell_types::JournalEnvelopeHook>,
    ) -> Self {
        self.journal_envelope_hook = Some(hook);
        self
    }

    /// Install a `JobEvent` observer. A detached agent runtime registers here
    /// (via the bin's `build_repl`) to forward its agent's lifecycle/terminal
    /// main REPL never calls this.
    pub fn with_job_event_observer(
        mut self,
        observer: std::sync::Arc<dyn orkia_shell_types::JobEventObserver>,
    ) -> Self {
        self.job_event_observer = Some(observer);
        self
    }

    /// Install the detached runtime's `AgentFinalResponse` forwarder
    /// `build_repl` only when this process is a detached runtime; the main REPL
    /// never calls this. A bus subscriber in `boot_journal_local` drives it.
    pub fn with_detached_afr_forwarder(
        mut self,
        forwarder: std::sync::Arc<dyn orkia_shell_types::JournalEnvelopeHook>,
    ) -> Self {
        self.detached_afr_forwarder = Some(forwarder);
        self
    }

    /// Install the daemon-jobs bridge so the bare-word `ps` / `wait` /
    /// `kill` / `tell` builtins surface daemon-owned detached jobs
    /// `build_repl` for the main REPL only; a detached runtime never
    /// calls this.
    pub fn with_daemon_jobs(
        mut self,
        bridge: std::sync::Arc<dyn orkia_shell_types::DaemonJobs>,
    ) -> Self {
        self.daemon_jobs = Some(bridge);
        self
    }

    /// Install the detached spawner so agent dispatch spawns through the
    /// `pty_daemon` (the agent survives REPL exit) instead of in-process
    /// `build_repl` for the main REPL ONLY; a detached runtime leaves it
    /// `None` (the `ORKIA_DETACHED_JOB_ID` recursion guard) and hosts the
    /// agent in-process via `JobController::spawn`.
    pub fn with_detached_spawner(
        mut self,
        spawner: std::sync::Arc<dyn orkia_shell_types::DetachedSpawner>,
    ) -> Self {
        self.detached_spawner = Some(spawner);
        self
    }

    /// Cloneable handle to the underlying job controller. Used by the
    /// Team pipeline coordinator (which lives in a separate crate) to
    /// spawn pipeline stages through the same machinery the REPL uses.
    pub fn jobs_handle(&self) -> &JobController {
        &self.jobs
    }

    /// Provide a factory the `tui` builtin can use to construct a fresh
    /// renderer when the user enters TUI mode. The binary supplies this
    /// because orkia-shell can't depend on orkia-shell-tui.
    pub fn with_tui_factory(mut self, factory: TuiFactory) -> Self {
        self.tui_factory = Some(factory);
        self
    }

    /// Mark this REPL as a login shell. Login shells source the
    /// `.bash_profile`/`.bash_login`/`.profile` chain; non-login shells
    /// source `.bashrc`. Determined by the binary from `argv[0]` /
    /// `--login`.
    pub fn with_login_shell(mut self, login: bool) -> Self {
        self.login_shell = login;
        self
    }

    /// Take the unified [`OrkiaEvent`] receiver. Returns `None` if
    /// already taken. Downstream consumers (Surface app, metrics,
    /// future team-sync) drive themselves off this stream — the
    /// REPL itself does not consume it in V1.
    pub fn take_orkia_event_rx(
        &mut self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<crate::protocol::OrkiaEvent>> {
        self.orkia_event_rx.take()
    }

    /// Read-only access to recorded history entries. Used by tests
    /// that need to confirm a `tick()` produced the expected
    /// `HistoryType` (e.g., ShellToAgent vs Shell vs AgentDelegation).
    pub fn history_snapshot(&self) -> &[orkia_shell_types::HistoryEntry] {
        self.history.entries()
    }

    /// Build the Orkia Cage wrapper for an agent spawn from the `[cage]`
    /// disabled or no policy is resolvable — agents then spawn bare, exactly
    /// as before. A leading `~/` in the global policy path is expanded to
    /// `$HOME` (the child `exec`s the path verbatim — no shell expansion).
    ///
    /// Policy resolution is **per agent**: a co-located
    /// `<data_dir>/agents/<agent>/policy.toml` (what `cap @agent` writes) takes
    /// precedence; the global `[cage].policy` is the fallback when no per-agent
    /// file exists. This is the storage substrate the per-agent `cap` model
    /// requires — `@faye` and `@rex` resolve to distinct files.
    pub(crate) fn cage_wrapper(&self, agent: &str) -> Option<crate::job::CageWrapper> {
        // A detached runtime receives the REPL's resolved cage on its env
        // (`runtime_spawn::build_env`) because it does not reliably re-derive
        // `[cage]` from config. Honor that first so the runtime cages the AGENT
        // it spawns in-process with exactly what the REPL decided.
        if let (Ok(cage_bin), Ok(policy_path)) = (
            std::env::var("ORKIA_DETACHED_CAGE_BIN"),
            std::env::var("ORKIA_DETACHED_CAGE_POLICY"),
        ) {
            return Some(crate::job::CageWrapper {
                cage_bin,
                policy_path,
            });
        }
        resolve_detached_cage(&self.config, agent).map(|(cage_bin, policy_path)| {
            crate::job::CageWrapper {
                cage_bin,
                policy_path,
            }
        })
    }

    /// The cage wrapper for `agent` as the transport type a [`DetachedSpawnRequest`]
    /// carries — so a daemon-owned spawn is caged by the REPL's decision rather
    /// than the runtime re-deriving (unreliably) from its own config.
    pub(crate) fn detached_cage(
        &self,
        agent: &str,
    ) -> Option<orkia_shell_types::DetachedCageWrapper> {
        self.cage_wrapper(agent)
            .map(|w| orkia_shell_types::DetachedCageWrapper {
                cage_bin: w.cage_bin,
                policy_path: w.policy_path,
            })
    }
}

/// Resolve the policy file an agent's cage runs under: the co-located per-agent
/// `<data_dir>/agents/<agent>/policy.toml` (what `cap @agent` writes) when it
/// exists, else the global `[cage].policy` (tilde-expanded). `None` when neither
/// exists — the caller then spawns the agent bare. Pure (only touches the
/// filesystem to test existence) so the per-agent-vs-global branch is unit-testable.
pub(crate) fn resolve_policy_path(
    data_dir: &std::path::Path,
    agent: &str,
    global: Option<&std::path::Path>,
) -> Option<String> {
    let per_agent = crate::agent_dir::agent_policy_path(data_dir, agent);
    if per_agent.is_file() {
        return Some(per_agent.to_string_lossy().into_owned());
    }
    Some(expand_tilde(global?))
}

/// Resolve the cage launcher + policy for `agent` from config alone (no `Repl`):
/// `(cage_bin, policy_path)`, or `None` when the cage is disabled or no policy
/// resolves. Shared by the REPL's `cage_wrapper` and the daemon's detached-spawn
/// fallback (`handle_spawn`) so EVERY detached path — bare `@agent`, `--detach
/// -c`, RFC dispatch — cages identically without each caller re-implementing it.
pub fn resolve_detached_cage(config: &ShellConfig, agent: &str) -> Option<(String, String)> {
    let cage = &config.cage;
    if !cage.enabled {
        return None;
    }
    let cage_bin = cage.bin.clone().unwrap_or_else(|| "orkia-cage".to_string());
    let policy_path = resolve_policy_path(&config.data_dir, agent, cage.policy.as_deref())?;
    Some((cage_bin, policy_path))
}

/// Expand a leading `~/` to `$HOME`. Other forms (bare `~`, `~user`) are
/// left untouched — V1 only needs the common `~/.orkia/...` case.
pub(crate) fn expand_tilde(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return std::path::Path::new(&home)
            .join(rest)
            .to_string_lossy()
            .into_owned();
    }
    s.into_owned()
}

#[cfg(test)]
mod tests {
    use super::resolve_policy_path;
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn per_agent_file_wins_over_global() {
        let tmp = TempDir::new().unwrap();
        let data = tmp.path();
        let faye = data.join("agents/faye");
        std::fs::create_dir_all(&faye).unwrap();
        std::fs::write(faye.join("policy.toml"), "x").unwrap();
        let global = data.join("global.toml");
        std::fs::write(&global, "x").unwrap();

        let got = resolve_policy_path(data, "faye", Some(&global)).unwrap();
        assert_eq!(got, faye.join("policy.toml").to_string_lossy());
    }

    #[test]
    fn falls_back_to_global_when_no_per_agent_file() {
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("global.toml");
        let got = resolve_policy_path(tmp.path(), "faye", Some(&global)).unwrap();
        assert_eq!(got, global.to_string_lossy());
    }

    #[test]
    fn none_when_neither_per_agent_nor_global() {
        let tmp = TempDir::new().unwrap();
        assert!(resolve_policy_path(tmp.path(), "faye", None).is_none());
    }

    #[test]
    fn two_agents_resolve_to_distinct_per_agent_files() {
        let tmp = TempDir::new().unwrap();
        let data = tmp.path();
        for a in ["faye", "rex"] {
            let d = data.join("agents").join(a);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("policy.toml"), "x").unwrap();
        }
        let faye = resolve_policy_path(data, "faye", None).unwrap();
        let rex = resolve_policy_path(data, "rex", None).unwrap();
        assert_ne!(faye, rex);
        assert!(faye.ends_with("agents/faye/policy.toml"));
        assert!(rex.ends_with("agents/rex/policy.toml"));
    }

    #[test]
    fn global_tilde_is_expanded() {
        if std::env::var_os("HOME").is_none() {
            return; // expand_tilde is a no-op without HOME; nothing to assert.
        }
        let tmp = TempDir::new().unwrap();
        // No per-agent file for "ghost", so resolution falls to the global path.
        let got = resolve_policy_path(tmp.path(), "ghost", Some(Path::new("~/x.toml"))).unwrap();
        assert!(!got.starts_with("~/"), "tilde must be expanded; got {got}");
    }
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Wire the OSS `@a | @b` coordinator into the REPL.
//!
//! The proxy is attached only when (a) the user's plan unlocks team
//! pipelines (`Capability::TeamPipeline`) and (b) an `orkia-kernel` daemon
//! is reachable. Absent either, the REPL
//! carries no coordinator and `@a | @b` returns the existing "requires
//! Orkia Team" message (fail-closed — CLAUDE.md). The shell never holds
//! premium logic: the kernel decides, [`KernelPipelineProxy`] drives the
//! PTY stages.

use std::sync::Arc;

use orkia_capabilities::{Capability, CapabilityResolver};
use orkia_pipeline_proxy::{KernelPipelineProxy, ResolvedRuntime, StageResolver};
use orkia_shell::ShellConfig;
use orkia_shell_types::{
    AgentPipelineCoordinator, FinalResponseSource, JournalEnvelopeHook, JournalStopHook,
};
use orkia_stage_exec::{StageContextProvider, StageExecConfig, StageExecutor};

/// Per-stage timeout when a plan carries none. Mirrors the legacy Team
const DEFAULT_STAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Everything `build_repl` needs to attach the coordinator. The same
/// final-response service backs both the REPL's Stop hook and the
/// executor's fallback capture, so there is one owner.
pub(crate) struct PipelineBundle {
    pub coordinator: Arc<dyn AgentPipelineCoordinator>,
    pub envelope_hook: Arc<dyn JournalEnvelopeHook>,
    pub stop_hook: Arc<dyn JournalStopHook>,
    pub source: Arc<dyn FinalResponseSource>,
    /// Receiver side of the bundle FRS's emit channel. `build_repl` hands
    /// it to the shell (`with_final_response_ingress`) so the local journal
    /// boot relays the emitted `AgentFinalResponse` envelopes into the hub
    /// ingress — bus fanout, disk tee, and the detached AFR forwarder all
    /// see the turn. Draining this to nowhere silently broke AFR
    /// forwarding for every Team-authed detached runtime.
    pub frs_rx: tokio::sync::mpsc::UnboundedReceiver<orkia_shell_types::JournalEnvelope>,
}

/// Build the coordinator bundle when the capability is unlocked and a
/// kernel is reachable; otherwise `None`. Call sites attach all four hooks
/// or none. `subscribed` is the journal topology: `true` when the daemon
/// hosts the hub (the REPL streams envelopes), `false` when the REPL owns
/// `orkia.sock` itself.
pub(crate) fn build(
    config: &ShellConfig,
    resolver: &Arc<dyn CapabilityResolver>,
    subscribed: bool,
) -> Option<PipelineBundle> {
    // Gate the `@a | @b` coordinator on the dedicated team-pipeline
    // capability (Team/Enterprise only — solo-pro gets cognitive routing
    // but not multi-agent chains). This pairs with the server-side team
    // membership check in `dispatch_pipeline` (dual-gating). The separate
    // classification kernel attach in `reconcile_kernel` stays on
    // `CognitiveRouting` so solo-pro keeps local routing.
    if !resolver.current().has(Capability::TeamPipeline) {
        return None;
    }
    let kernel = orkia_kernel_client::discover()?;

    let (frs_tx, frs_rx) = tokio::sync::mpsc::unbounded_channel();
    let frs =
        orkia_final_response::FinalResponseService::new(config.data_dir.clone(), frs_tx).into_arc();
    // The stage waiter subscribes to `source`. In subscribed mode the
    // daemon owns the stop hook, so this bundle's FRS never sees a `Stop`
    // — the source must be the passive one `boot_journal_subscribed` feeds
    // from the streamed `AgentFinalResponse` envelopes. In local mode the
    // bundle's FRS becomes the shell's stop hook and fires directly.
    let source: Arc<dyn FinalResponseSource> = if subscribed {
        Arc::new(orkia_shell::repl::StreamedFinalResponseSource::default())
    } else {
        frs.clone()
    };

    let executor = Arc::new(StageExecutor::new(StageExecConfig {
        data_dir: config.data_dir.clone(),
        socket_path: config.data_dir.join("run").join("orkia.sock"),
        final_response_source: source.clone(),
        context_provider: Arc::new(ConfigContextProvider::from_config(config)),
        default_stage_timeout: DEFAULT_STAGE_TIMEOUT,
    }));

    let stage_resolver: Arc<dyn StageResolver> = Arc::new(ConfigResolver::from_config(config));
    let proxy = Arc::new(KernelPipelineProxy::new(
        kernel,
        stage_resolver,
        executor.clone(),
    ));

    Some(PipelineBundle {
        coordinator: proxy,
        envelope_hook: executor,
        stop_hook: frs,
        source,
        frs_rx,
    })
}

/// Resolves `@agent → command/args/provider` from the shell's agent map.
/// The provider is derived from the command basename so the kernel can make
/// MCP-wiring decisions without reading the agent registry.
struct ConfigResolver {
    agents: std::collections::HashMap<String, ResolvedRuntime>,
}

impl ConfigResolver {
    fn from_config(config: &ShellConfig) -> Self {
        let mut agents: std::collections::HashMap<String, ResolvedRuntime> = config
            .agent_commands
            .iter()
            .map(|(name, cmd)| {
                (
                    name.clone(),
                    ResolvedRuntime {
                        command: cmd.command.clone(),
                        args: cmd.args.clone(),
                        provider: provider_from_command(&cmd.command),
                        runtime: None,
                    },
                )
            })
            .collect();
        // Native agents live outside `agent_commands` (no vendor CLI to
        // resolve — see `hydrate_agents_from_dir`): overlay them from
        // their definitions so a pipeline stage can route to the native
        // executor instead of refusing as "no command configured".
        for def in orkia_shell::agent_dir::load_all_definitions(&config.data_dir) {
            if let orkia_shell_types::AgentRuntimeKind::Native { model } = def.runtime {
                agents.insert(
                    def.name,
                    ResolvedRuntime {
                        command: String::new(),
                        args: Vec::new(),
                        provider: None,
                        runtime: Some(model),
                    },
                );
            }
        }
        Self { agents }
    }
}

impl StageResolver for ConfigResolver {
    fn resolve(&self, agent: &str) -> Option<ResolvedRuntime> {
        self.agents.get(agent).cloned()
    }
}

/// Assembles a stage agent's context the same way Solo dispatch's
/// `build_agent_context` does: load the agent definition + workspace and
/// run `AgentContext::load_with_filter`. The scope filter is permissive on
/// auth/team — this provider is only constructed once `TeamPipeline` is
/// unlocked, so the user is authenticated and team-bearing by construction.
/// Intelligence enrichment stays REPL-owned and is intentionally omitted.
struct ConfigContextProvider {
    data_dir: std::path::PathBuf,
    workspace: orkia_shell_types::Workspace,
    workspace_default: Option<orkia_shell_types::Scope>,
}

impl ConfigContextProvider {
    fn from_config(config: &ShellConfig) -> Self {
        Self {
            data_dir: config.data_dir.clone(),
            workspace: orkia_shell_types::Workspace::load(&config.data_dir),
            workspace_default: config.default_scope,
        }
    }
}

impl StageContextProvider for ConfigContextProvider {
    fn context_for(&self, agent: &str) -> Option<orkia_shell::agent_context::AgentContext> {
        let def = orkia_shell::agent_dir::load_definition_by_name(&self.data_dir, agent)?;
        let filter = orkia_shell::agent_context::ScopeFilterContext {
            has_team_membership: true,
            is_authenticated: true,
            workspace_default: self.workspace_default,
        };
        Some(orkia_shell::agent_context::AgentContext::load_with_filter(
            &def,
            &self.workspace,
            &filter,
        ))
    }
}

/// Map a command to its MCP provider by basename. Providers without
/// hook capture return `None`, which the kernel rejects (pipelines need
/// Stop-hook capture for stage output).
fn provider_from_command(command: &str) -> Option<String> {
    let id = orkia_shell_types::ProviderId::from_command(command);
    id.capabilities()
        .hooks_capture
        .then(|| id.as_str().to_string())
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Wire the OSS RFC→many-agents dispatch coordinator into the REPL.
//!
//! Sibling of [`crate::pipeline_wiring`]: the proxy is attached only when (a)
//! the plan unlocks team pipelines (`Capability::TeamPipeline`, dual-gated with
//! the server-side membership check in `handle_rfc_dispatch`) and (b) the three
//! daemon-owned execution seams are present — the fan-out spawner, the fan-in
//! final-response source, and the daemon-jobs liveness view used on resume.
//! Absent any of these the slot stays empty and `orkia rfc dispatch` surfaces
//! the Team-required block (fail-closed — CLAUDE.md §8). The shell never holds
//! premium logic: the kernel owns the DAG, the proxy drives PTY execution.

use std::sync::Arc;

use orkia_capabilities::{Capability, CapabilityResolver};
use orkia_dispatch_proxy::{
    AgentResolver, Clock, DispatchSeams, KernelDispatchProxy, ResolvedRuntime,
};
use orkia_shell::ShellConfig;
use orkia_shell_types::{DaemonJobs, DetachedSpawner, FinalResponseSource};

/// Everything `build_repl` hands the dispatch wiring. The final-response
/// `source` is cloned from the pipeline bundle so both coordinators observe
/// the same Stop-hook capture stream (one owner); the spawner and daemon-jobs
/// seams are the same Arcs attached to the REPL.
pub(crate) struct DispatchWiring<'a> {
    pub config: &'a ShellConfig,
    pub resolver: &'a Arc<dyn CapabilityResolver>,
    pub source: Option<Arc<dyn FinalResponseSource>>,
    pub spawner: Option<Arc<dyn DetachedSpawner>>,
    pub daemon_jobs: Option<Arc<dyn DaemonJobs>>,
}

/// Build the dispatch coordinator when the capability is unlocked, a kernel is
/// reachable, and all three execution seams are present; otherwise `None`.
pub(crate) fn build(w: DispatchWiring<'_>) -> Option<Arc<KernelDispatchProxy>> {
    if !w.resolver.current().has(Capability::TeamPipeline) {
        return None;
    }
    // Fan-out, fan-in, and resume-liveness are all daemon-owned. A detached
    // runtime has none of them (the `ORKIA_DETACHED_JOB_ID` recursion guard)
    // and never starts a run — it only re-runs a single task — so require all
    // three rather than driving a run that can't spawn or reconcile.
    let source = w.source?;
    let spawner = w.spawner?;
    let daemon_jobs = w.daemon_jobs?;
    let kernel = orkia_kernel_client::discover()?;

    let resolver: Arc<dyn AgentResolver> = Arc::new(DispatchResolver::from_config(w.config));
    // The run's `started` stamp. The proxy never reads the clock itself so its
    // runs stay deterministic in tests; the command surface owns time here.
    let clock: Clock = Arc::new(|| chrono::Utc::now().to_rfc3339());
    Some(Arc::new(KernelDispatchProxy::new(
        kernel,
        resolver,
        DispatchSeams {
            spawner,
            responses: source,
            daemon_jobs,
            clock,
        },
    )))
}

/// Resolves `@agent → command/args/provider/runtime` from the shell's agent
/// map. Mirror of `pipeline_wiring::ConfigResolver` for the dispatch crate's
/// own `AgentResolver` trait + `ResolvedRuntime` type (the two live behind a
/// crate boundary, so the resolver can't be shared without a third type).
struct DispatchResolver {
    agents: std::collections::HashMap<String, ResolvedRuntime>,
}

impl DispatchResolver {
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
        // Native agents live outside `agent_commands` — overlay them from their
        // definitions so a task can route to the native executor instead of
        // refusing as "no command configured".
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

impl AgentResolver for DispatchResolver {
    fn resolve(&self, agent: &str) -> Option<ResolvedRuntime> {
        self.agents.get(agent).cloned()
    }
}

/// Map a command to its MCP provider by basename. Providers without hook
/// capture return `None`, which the kernel rejects (tasks need Stop-hook
/// capture for their response). Mirror of the pipeline wiring's helper.
fn provider_from_command(command: &str) -> Option<String> {
    let id = orkia_shell_types::ProviderId::from_command(command);
    id.capabilities()
        .hooks_capture
        .then(|| id.as_str().to_string())
}

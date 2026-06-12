// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use orkia_auth::AuthProvider;
use orkia_capabilities::{
    Capability, CapabilityResolver, PlanResolver, PlanSource, ProviderPlanSource,
};
use orkia_magic_login::MagicLinkAuthProvider;
use orkia_shell::renderer::ShellRenderer;
use orkia_shell::repl::TuiFactory;
use orkia_shell::{AdaptiveClassifier, AdaptiveHandle, HeuristicRouter, Repl, ShellConfig};
use orkia_shell_tui::TuiRenderer;
use orkia_shell_types::backend::{DEFAULT_BACKEND_URL, resolve_backend_url};
use std::sync::Arc;

/// plan resolver up front so every `Repl::new` site wires them
/// identically. Pre-attaches the kernel when the user's current plan
/// already unlocks it — first `$login` doesn't have to do it.
pub(crate) fn build_capability_wiring() -> (
    AdaptiveClassifier,
    AdaptiveHandle,
    Arc<dyn CapabilityResolver>,
    Arc<dyn AuthProvider>,
) {
    let base = resolve_backend_url(None).unwrap_or_else(|_| DEFAULT_BACKEND_URL.to_string());
    let provider: Arc<dyn AuthProvider> = Arc::new(MagicLinkAuthProvider::new(base));
    let source: Arc<dyn PlanSource> = Arc::new(ProviderPlanSource::new(provider.clone()));
    let resolver: Arc<dyn CapabilityResolver> = Arc::new(PlanResolver::new(source));
    let classifier = AdaptiveClassifier::heuristic_only();
    let handle = classifier.handle();
    reconcile_kernel(&resolver, &handle);
    spawn_capability_poller(resolver.clone(), handle.clone());
    (classifier, handle, resolver, provider)
}

/// Inspect the current capability set and (de)attach the kernel
/// handle accordingly. Cheap to call — bails out fast when nothing
/// changed.
pub(crate) fn reconcile_kernel(resolver: &Arc<dyn CapabilityResolver>, handle: &AdaptiveHandle) {
    let caps = resolver.current();
    if caps.has(Capability::CognitiveRouting) {
        if !handle.has_kernel()
            && let Some(rpc) = orkia_kernel_client::discover()
        {
            handle.set_kernel(rpc);
        }
    } else if handle.has_kernel() {
        handle.clear_kernel();
    }
}

/// store every 60s + reconcile the kernel handle live. Drives plan
/// downgrade detection without a shell restart and without forcing
/// the user to re-run `$login`.
pub(crate) fn spawn_capability_poller(
    resolver: Arc<dyn CapabilityResolver>,
    handle: AdaptiveHandle,
) {
    let interval = std::time::Duration::from_secs(60);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await; // skip the immediate fire — initial state is already wired
        loop {
            ticker.tick().await;
            resolver.refresh();
            reconcile_kernel(&resolver, &handle);
        }
    });
}

/// Build a closure that constructs a TuiRenderer on demand. Installed
/// onto the REPL so the `tui` builtin can swap renderers at runtime
/// without orkia-shell taking a dep on orkia-shell-tui.
pub(crate) fn make_tui_factory() -> TuiFactory {
    Box::new(|agents, workspace| {
        TuiRenderer::new(agents.to_vec(), workspace.clone())
            .map(|r| Box::new(r) as Box<dyn ShellRenderer>)
            .map_err(|e| orkia_shell::ShellError::Other(format!("{e}")))
    })
}

/// Wiring arguments shared by every `build_repl` call site.
pub(crate) struct ReplWiring {
    pub(crate) classifier: AdaptiveClassifier,
    pub(crate) config: ShellConfig,
    pub(crate) login: bool,
    pub(crate) resolver: Arc<dyn CapabilityResolver>,
    pub(crate) handle: AdaptiveHandle,
    pub(crate) auth: Arc<dyn AuthProvider>,
    /// `Some(factory)` for renderers that support runtime TUI switching
    /// (ShellMode); `None` for Stdout and TuiRenderer.
    pub(crate) tui_factory: Option<TuiFactory>,
}

/// Apply the common REPL builder chain shared by every renderer variant.
pub(crate) fn build_repl<R: ShellRenderer>(renderer: R, w: ReplWiring) -> Repl {
    // hub before constructing the REPL. On success `boot_journal` runs in
    // subscribed mode (the daemon owns `orkia.sock`, FRS + disk tee survive a
    // REPL restart); on failure the REPL boots its own local hub. Computed
    // here because `w.config` is moved into `Repl::new` below — and FIRST,
    // because the pipeline wiring picks its final-response source by topology.
    let pump_starter = crate::pty_daemon::journal_pump_starter(&w.config);
    // Attach the OSS @a|@b coordinator when the plan unlocks the kernel and
    // a daemon is reachable; otherwise the slot stays empty and pipelines
    // return the Team-required message (fail-closed).
    let pipeline = crate::pipeline_wiring::build(&w.config, &w.resolver, pump_starter.is_some());
    // Same dual gate for Forge: kernel-relayed builder when ForgeBuild is
    // unlocked + a daemon is up, else the no-op (premium-required message).
    let forge: Arc<dyn orkia_shell_types::ForgeBuilder> =
        crate::forge_wiring::build(&w.resolver, &w.auth)
            .unwrap_or_else(|| Arc::new(orkia_shell::NoopForgeBuilder::new()));
    // Same dual gate for the per-RFC SEAL v1 assembler: kernel-relayed
    // when SealAuditExtended is unlocked + a daemon is up, else left unwired
    // (`rfc seal` reports "not wired").
    let seal = crate::seal_wiring::build(&w.resolver);
    // JobEvent observer that forwards its agent's lifecycle events up to the
    // daemon (which relays them to the main REPL). `None` for the main REPL.
    // Computed before `w.config` is moved into `Repl::new`.
    let job_event_observer = crate::pty_daemon::job_forward::observer(&w.config);
    // main REPL sees the turn. `None` for the main REPL. Same pre-move timing.
    let afr_forwarder = crate::pty_daemon::job_forward::afr_forwarder(&w.config);
    // `kill`/`tell` builtins surface daemon-owned detached jobs (a detached
    // runtime gets `None` — it never recurses into its own daemon).
    let daemon_jobs = crate::pty_daemon::daemon_jobs::provider(&w.config);
    // daemon (the agent survives REPL exit). A detached runtime gets `None` (the
    // recursion guard) and hosts the agent in-process — it is the daemon's host.
    let detached_spawner = crate::pty_daemon::detached_spawner::provider(&w.config);
    let repl = Repl::new(renderer, w.classifier, HeuristicRouter, w.config)
        .with_login_shell(w.login)
        .with_capabilities(w.resolver, w.handle)
        .with_auth_provider(w.auth)
        .with_forge_builder(forge);
    let repl = match job_event_observer {
        Some(obs) => repl.with_job_event_observer(obs),
        None => repl,
    };
    let repl = match afr_forwarder {
        Some(fwd) => repl.with_detached_afr_forwarder(fwd),
        None => repl,
    };
    let repl = match daemon_jobs {
        Some(bridge) => repl.with_daemon_jobs(bridge),
        None => repl,
    };
    let repl = match detached_spawner {
        Some(spawner) => repl.with_detached_spawner(spawner),
        None => repl,
    };
    let repl = match seal {
        Some(s) => repl.with_seal_assembler(s),
        None => repl,
    };
    let repl = match pipeline {
        Some(p) => repl
            .with_pipeline_coordinator(p.coordinator)
            .with_journal_envelope_hook(p.envelope_hook)
            .with_final_response_hook(p.stop_hook)
            .with_final_response_source(p.source)
            // The bundle FRS's emit channel: the local journal boot relays
            // it into the hub ingress so AFR envelopes reach the bus (and
            // a detached runtime's AFR forwarder) like the default FRS's.
            .with_final_response_ingress(p.frs_rx),
        None => repl,
    };
    let mut repl = match w.tui_factory {
        Some(f) => repl.with_tui_factory(f),
        None => repl,
    };
    if let Some(starter) = pump_starter {
        repl.set_journal_pump_starter(starter);
    }
    repl
}

/// Run a REPL, printing a fatal error and exiting with code 1 on failure.
pub(crate) async fn run_repl(mut repl: Repl) {
    if let Err(e) = repl.run().await {
        eprintln!("orkia: fatal: {e}");
        std::process::exit(1);
    }
}

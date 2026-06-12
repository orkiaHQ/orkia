// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `JobLifecycleHook` — the trait the [`super::JobController::spawn`]
//! dispatcher fires on every attachment, and the four concrete impls
//! that wrap today's subsystems (state machine, injection executor,
//! SEAL chain, OSC 133 listener).
//!
//! Hooks are stored as `Arc<dyn JobLifecycleHook>` on the
//! [`super::entry::JobEntry`] so the controller can dispatch
//! `on_complete` after the per-job state is otherwise torn down.
//! All hook methods take `&self`; each impl holds a cheap-clone
//! handle to its singleton subsystem (channel sender or
//! interior-mutable struct).
//!

use std::sync::Arc;

use orkia_shell_types::JobId;
use orkia_terminal_core::TerminalEngine;

use crate::injection_executor::{InjectionExecutor, output_transcript_probe};
use crate::protocol::EventRouter;
use crate::terminal_state::TerminalStateMachine;

/// Per-job spawn context handed to each [`JobLifecycleHook`]. The
/// hook reads what it needs (engine writer, pid, optional initial
/// body for state-machine queuing) and ignores the rest.
pub struct SpawnContext<'a> {
    pub job_id: JobId,
    pub agent_name: &'a str,
    pub pid: Option<u32>,
    pub engine: &'a TerminalEngine,
    /// Hashes captured from any `Attachment::AgentContext`. Empty
    /// strings + zero count when there's no agent context (plain
    /// shell job). The SEAL hook reads these to enrich the
    /// `agent.spawn` event.
    pub system_prompt_hash: &'a str,
    pub memory_hash: &'a str,
    pub tools_count: usize,
}

/// Per-job lifecycle hook installed by an `Attachment`. Fires once
/// at spawn (immediately after the engine is up and the entry is
/// registered) and once at completion (from the controller's
/// [`super::JobController::dispatch_on_complete`]).
///
/// Implementors should treat both methods as idempotent — the
/// controller guarantees `on_spawn` fires at most once and
/// `on_complete` at most once, but mis-wired tests have produced
/// double-fires in the past and the cost of being defensive is
/// negligible.
pub trait JobLifecycleHook: Send + Sync {
    fn on_spawn(&self, ctx: &SpawnContext<'_>);
    fn on_complete(&self, job_id: JobId, exit_code: i32);
}

// =============================================================
// StateMachineLifecycle — prompt detector registration
// =============================================================

/// Wraps the per-shell `TerminalStateMachine` singleton. On spawn,
/// registers the job (which spawns a detector thread); on complete,
/// removes the job (joining the detector thread). The dropped
/// pending body is surfaced through the state machine's normal
/// `cleanup` path on the next REPL drain — the hook itself doesn't
/// forward it (the REPL still owns the prompt-dropped notification).
pub struct StateMachineLifecycle {
    sm: TerminalStateMachine,
    /// `Some(body)` when the controller did not write an initial
    /// prompt — the state machine queues this so the detector can
    /// inject it once the agent's ready prompt fires. `None` when
    /// the hook-driven agent path wrote the intent directly to PTY
    /// at spawn (see `dispatch_agent::initial_prompt`).
    pending_body: Option<String>,
}

impl StateMachineLifecycle {
    pub fn new(sm: TerminalStateMachine, pending_body: Option<String>) -> Self {
        Self { sm, pending_body }
    }
}

impl JobLifecycleHook for StateMachineLifecycle {
    fn on_spawn(&self, ctx: &SpawnContext<'_>) {
        // No pid → no detector. This matches today's behavior in
        // dispatch_agent: the `Some(pid)` guard skipped registration
        // for spawns where portable-pty couldn't surface a child id.
        let Some(pid) = ctx.pid else { return };
        self.sm.register_agent_job(
            ctx.job_id,
            ctx.agent_name,
            pid,
            ctx.engine,
            self.pending_body.clone(),
        );
    }

    fn on_complete(&self, _job_id: JobId, _exit_code: i32) {
        // The REPL calls `state_machine.remove_job` explicitly in
        // its `drain_job_events` `Completed` arm so it can pick up
        // the returned `DroppedPrompt` and surface a user-visible
        // toast. Doing it here too would double-join the detector
        // thread. Left as a no-op for symmetry with the spawn
        // hook; if a future caller spawns a state-machine-attached
        // job from outside the REPL it can call `remove_job`
        // itself on completion.
    }
}

// =============================================================
// InjectionExecutorLifecycle — async PTY writer registration
// =============================================================

/// Hands the executor a clone of the engine's PTY writer at spawn,
/// and drops it at completion so the master fd can close cleanly.
pub struct InjectionExecutorLifecycle {
    exec: InjectionExecutor,
    provider: orkia_shell_types::ProviderId,
}

impl InjectionExecutorLifecycle {
    pub fn new(exec: InjectionExecutor, provider: orkia_shell_types::ProviderId) -> Self {
        Self { exec, provider }
    }
}

impl JobLifecycleHook for InjectionExecutorLifecycle {
    fn on_spawn(&self, ctx: &SpawnContext<'_>) {
        // The probe lets the executor confirm a typed body landed in the
        // agent's input box before submitting (Enter). Codex currently
        // exposes the composer reliably in the output stream even when the
        // alacritty grid snapshot is blank, so use a transcript-backed probe
        // for it and keep the grid probe for the other TUIs.
        let probe = if self.provider == orkia_shell_types::ProviderId::Codex {
            output_transcript_probe(ctx.engine.subscribe_output())
        } else {
            ctx.engine.grid_probe()
        };
        self.exec
            .register(ctx.job_id, ctx.engine.writer(), Some(probe));
    }

    fn on_complete(&self, job_id: JobId, _exit_code: i32) {
        self.exec.unregister(job_id);
    }
}

// =============================================================
// SealChainLifecycle — agent.spawn / agent.complete genesis events
// =============================================================

/// Emits the `agent.spawn` custom event on spawn (only when the
/// associated `AgentContext` produced non-empty hashes — a bare
/// shell job carries empty hashes and no chain). On completion the
/// SEAL chain is closed via the `SessionEnd` event the REPL still
/// emits in its `Completed` arm; we don't need to fire a separate
/// `agent.complete` here.
///
/// `project` is the optional project link recorded in the REPL's
/// `JobProjects` map (today inserted directly in
/// `dispatch_rfc_delegate`); when present the SEAL consumer writes
/// a `job.reference` record into the project chain on close.
pub struct SealChainLifecycle {
    router: EventRouter,
    project: Option<String>,
    /// REPL-shared map. The hook writes the project membership
    /// into it BEFORE emitting `agent.spawn` so the consumer sees
    /// the link by the time it processes the genesis event (see
    job_projects: Arc<parking_lot::RwLock<std::collections::HashMap<JobId, String>>>,
    agent_name: String,
}

impl SealChainLifecycle {
    pub fn new(
        router: EventRouter,
        project: Option<String>,
        job_projects: Arc<parking_lot::RwLock<std::collections::HashMap<JobId, String>>>,
        agent_name: String,
    ) -> Self {
        Self {
            router,
            project,
            job_projects,
            agent_name,
        }
    }
}

impl JobLifecycleHook for SealChainLifecycle {
    fn on_spawn(&self, ctx: &SpawnContext<'_>) {
        // Order matters: record project membership first so the
        // SEAL consumer sees the link by the time it processes
        // the `agent.spawn` event.
        if let Some(project) = &self.project {
            self.job_projects
                .write()
                .insert(ctx.job_id, project.clone());
        }
        if ctx.system_prompt_hash.is_empty() {
            // No agent context → no chain. Plain shell jobs hit
            // this branch (and stay unsealed for now; future
            // SealChain-without-AgentContext use cases will plumb
            // a different event).
            return;
        }
        self.router.on_custom(
            ctx.job_id,
            &self.agent_name,
            "agent.spawn",
            serde_json::json!({
                "job_id": ctx.job_id.0,
                "agent": &self.agent_name,
                "system_prompt_hash": ctx.system_prompt_hash,
                "memory_hash": ctx.memory_hash,
                "tools_count": ctx.tools_count,
            }),
        );
    }

    fn on_complete(&self, _job_id: JobId, _exit_code: i32) {
        // No-op. The REPL's `drain_job_events` Completed arm
        // synthesizes the `SessionEnd` OrkiaProtocol event that
        // closes the chain — having the hook do it too would be
        // a double-close. Left here for symmetry / future use.
    }
}

// =============================================================
// Osc133Lifecycle — marker, not actual wiring
// =============================================================

/// OSC 133 + Orkia APC callbacks are installed on the
/// `TerminalEngine`'s `BlockParser` at engine-construction time
/// — they cannot be added post-spawn. This hook is therefore a
/// **marker**: the controller's spawn function checks for the
/// attachment when building `EngineConfig` and skips installing
/// the callbacks when the marker is absent. The hook itself does
/// nothing at runtime.
pub struct Osc133Lifecycle;

impl JobLifecycleHook for Osc133Lifecycle {
    fn on_spawn(&self, _ctx: &SpawnContext<'_>) {}
    fn on_complete(&self, _job_id: JobId, _exit_code: i32) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JobController;
    use crate::injection_executor::InjectionExecutor;
    use crate::protocol::EventRouter;
    use crate::terminal_state::TerminalStateMachine;
    use parking_lot::RwLock;
    use std::collections::HashMap;

    fn make_event_router_with_rx() -> (
        EventRouter,
        tokio::sync::mpsc::UnboundedReceiver<crate::protocol::OrkiaEvent>,
    ) {
        let router = EventRouter::new();
        let rx = router.take_rx().expect("rx");
        (router, rx)
    }

    #[test]
    fn injection_executor_lifecycle_unregister_is_safe_for_unknown_job() {
        // Verifies on_complete for an InjectionExecutorLifecycle
        // is a silent no-op when the executor has no writer for
        // this id (a future job kind that wires this hook but
        // never registered a writer must not panic).
        let exec = InjectionExecutor::spawn();
        let hook = InjectionExecutorLifecycle::new(exec, orkia_shell_types::ProviderId::Generic);
        hook.on_complete(JobId(9999), 0);
    }

    #[test]
    fn job_ids_never_recycle_so_stale_completion_cant_alias_a_reuse() {
        // Regression: `reap()` used to reset `next_id` to 1 when the job
        // table emptied (bash-style small numbers). Combined with the
        // async `Completed { id } -> remove_job(id)` teardown, an agent
        // that recycled a just-freed id would have its freshly-queued
        // body ripped out by the *previous* instance's late completion
        // (observed live: `@faye say byee` dropped with "state:
        // WaitingForBoot" right after a Ctrl-C'd faye was reaped). Ids
        // must be strictly monotonic so a stale teardown can never alias
        // a newer owner of the same id.
        let (mut jobs, _rx) = JobController::new();
        let first = jobs.alloc_id().unwrap();
        // Table is empty (no live entries) — the old code recycled here.
        jobs.reap();
        let second = jobs.alloc_id().unwrap();
        assert!(
            second.0 > first.0,
            "JobId recycled ({first:?} -> {second:?}): a late Completed for the \
             freed id would tear down the new instance that reused it",
        );
    }

    #[test]
    fn seal_chain_lifecycle_records_project_membership_before_event() {
        // When `project = Some(_)`, on_spawn must insert into
        // `job_projects` BEFORE emitting the agent.spawn custom
        // event. We can't directly observe the event ordering from
        // a test (the SEAL consumer thread isn't running), but we
        // verify the map insert happens on the spawn hook even
        // when the hashes are empty (which would otherwise skip
        // the agent.spawn emission).
        let (router, _rx) = make_event_router_with_rx();
        let projects: Arc<RwLock<HashMap<JobId, String>>> = Arc::new(RwLock::new(HashMap::new()));
        let hook = SealChainLifecycle::new(
            router,
            Some("test-project".into()),
            Arc::clone(&projects),
            "test-agent".into(),
        );
        // No engine needed for this test — the hook's on_spawn
        // doesn't dereference it. We construct a stub
        // SpawnContext via a real engine to satisfy the borrow.
        let cfg = orkia_terminal_core::EngineConfig {
            cmd: Some("/bin/sh".into()),
            args: vec!["-c".into(), "exit 0".into()],
            ..Default::default()
        };
        let engine = orkia_terminal_core::TerminalEngine::start(cfg).expect("engine");
        let ctx = SpawnContext {
            job_id: JobId(42),
            agent_name: "test-agent",
            pid: engine.child_id(),
            engine: &engine,
            system_prompt_hash: "",
            memory_hash: "",
            tools_count: 0,
        };
        hook.on_spawn(&ctx);
        assert_eq!(
            projects.read().get(&JobId(42)).map(String::as_str),
            Some("test-project"),
            "project membership must be recorded before any event",
        );
    }

    #[test]
    fn state_machine_lifecycle_registers_detector_on_spawn() {
        let sm = TerminalStateMachine::new();
        // pending_body=None: matches the hook-driven agent path.
        let hook = StateMachineLifecycle::new(sm.clone(), None);
        let cfg = orkia_terminal_core::EngineConfig {
            cmd: Some("/bin/sh".into()),
            args: vec!["-c".into(), "sleep 1".into()],
            ..Default::default()
        };
        let engine = orkia_terminal_core::TerminalEngine::start(cfg).expect("engine");
        let ctx = SpawnContext {
            job_id: JobId(7),
            agent_name: "test-agent",
            pid: engine.child_id(),
            engine: &engine,
            system_prompt_hash: "",
            memory_hash: "",
            tools_count: 0,
        };
        hook.on_spawn(&ctx);
        // The detector was registered — appending a follow-up
        // body should succeed (returns false when no entry).
        assert!(sm.append_body(JobId(7), "hi".into()));
        hook.on_complete(JobId(7), 0);
        // The hook's on_complete is a no-op (REPL does
        // remove_job explicitly), so the entry still exists.
        // Verifying explicit cleanup works:
        let _ = sm.remove_job(JobId(7));
    }

    #[test]
    fn osc133_lifecycle_is_a_pure_marker_noop() {
        let cfg = orkia_terminal_core::EngineConfig {
            cmd: Some("/bin/sh".into()),
            args: vec!["-c".into(), "exit 0".into()],
            ..Default::default()
        };
        let engine = orkia_terminal_core::TerminalEngine::start(cfg).expect("engine");
        let ctx = SpawnContext {
            job_id: JobId(1),
            agent_name: "",
            pid: engine.child_id(),
            engine: &engine,
            system_prompt_hash: "",
            memory_hash: "",
            tools_count: 0,
        };
        Osc133Lifecycle.on_spawn(&ctx);
        Osc133Lifecycle.on_complete(JobId(1), 0);
    }
}

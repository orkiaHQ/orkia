// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `JobController::spawn(JobConfig)` — the unified PTY-backed
//! spawn path. The REPL's agent dispatch and any future
//!
//! Responsibilities:
//!   1. Allocate a job id and per-job run directory.
//!   2. Walk the attachment list, splitting it into:
//!        - spawn-time bookkeeping (`Hooks`, `AgentContext`,
//!          `SealChain { project }`) that mutates `env` / `args` /
//!          `cwd` or installs config files,
//!        - engine-config flags (`Osc133Listener`),
//!        - lifecycle hooks (`StateMachine`, `InjectionExecutor`,
//!          `SealChain`) that fire on spawn / complete.
//!   3. Build the engine, start it, emit `Spawned`.
//!   4. Construct the `JobEntry` carrying the lifecycle hooks.
//!   5. Dispatch `on_spawn` to each hook in insertion order.
//!   6. Write the initial-bytes stdin if requested.

use std::sync::Arc;

use orkia_shell_types::StdinSource;
use orkia_shell_types::job::{JobId, JobKind, JobState};
use orkia_terminal_core::{EngineConfig, TerminalEngine};

use crate::approval::ApprovalWatcher;
use crate::error::ShellError;
use crate::job::config::{Attachment, CageWrapper, JobConfig};
use crate::job::entry::JobEntry;
use crate::job::lifecycle::{
    InjectionExecutorLifecycle, JobLifecycleHook, Osc133Lifecycle, SealChainLifecycle,
    SpawnContext, StateMachineLifecycle,
};
use crate::protocol::EventRouter;

use super::util::{inject_agent_context, install_hooks_for_provider, terminal_dims};
use super::{JobEvent, SpawnResult};

/// Per-spawn handles the REPL hands the controller for hook
/// construction. Bundled so [`spawn`] keeps a sane signature.
pub struct SpawnDeps<'a> {
    pub approvals: &'a ApprovalWatcher,
    pub event_router: &'a EventRouter,
    pub state_machine: &'a crate::terminal_state::TerminalStateMachine,
    pub injection_executor: &'a crate::injection_executor::InjectionExecutor,
    pub job_projects: &'a Arc<parking_lot::RwLock<std::collections::HashMap<JobId, String>>>,
    /// Agent name attribution for OSC 133 / SEAL events. For plain
    /// shell jobs this is the empty string.
    pub agent_name: &'a str,
}

/// Internal: per-spawn outcome of walking the attachment list.
struct AttachmentPlan {
    /// Hooks to install on the entry, in insertion order.
    hooks: Vec<Arc<dyn JobLifecycleHook>>,
    /// Whether to wire OSC 133 / APC callbacks on the engine.
    osc133: bool,
    /// Hashes derived from `Attachment::AgentContext`. Empty when
    /// absent (plain shell job).
    system_prompt_hash: String,
    memory_hash: String,
    tools_count: usize,
}

/// The actual unified spawn. Lives outside `impl JobController` so
/// the function signature stays under the file's 50-line per-fn
/// budget (the body delegates to small helpers).
pub(super) fn spawn(
    controller: &mut super::JobController,
    config: JobConfig<'_>,
    deps: SpawnDeps<'_>,
) -> Result<SpawnResult, ShellError> {
    let id = controller.alloc_id()?;
    let (cols, rows) = terminal_dims();
    let run_dir = deps.approvals.create_job_dir(id);
    // Global hub socket: `<data_dir>/run/orkia.sock` (run_dir is
    // `<data_dir>/run/<id>`, so its parent is the run dir).
    let global_socket = run_dir
        .parent()
        .map(|p| p.join("orkia.sock"))
        .unwrap_or_else(|| run_dir.join("orkia.sock"));
    // this agent's hooks/MCP at the runtime's per-job local hub socket so the
    // runtime consumes them itself instead of routing to the daemon's global
    // hub. `<data_dir>` is two levels up from the per-job run dir.
    let socket_path = run_dir
        .parent()
        .and_then(|run| run.parent())
        .and_then(crate::detached_control::detached_runtime_hub_socket)
        .unwrap_or(global_socket);

    let has_agent_context = config.has_agent_context();
    let label = config.label.clone();
    let working_dir = config.working_dir.clone();
    let stdin_source = config.stdin;

    let mut env = config.env;
    let mut args_owned: Vec<String> = config.args.to_vec();

    env.push((
        "ORKIA_RUN_DIR".into(),
        run_dir.to_string_lossy().into_owned(),
    ));
    env.push(("ORKIA_JOB_ID".into(), format!("{}", id.0)));
    // Tell the agent's hook shim (`orkia bridge`, a child of the agent process)
    // which hub socket to post to. In a detached runtime this is the per-job
    // `agent.sock` resolved above, so the runtime consumes its OWN agent's
    // `orkia.sock` — identical to the shim's previous hardcoded default, so
    // non-detached spawns are unchanged.
    env.push((
        "ORKIA_SOCKET_PATH".into(),
        socket_path.to_string_lossy().into_owned(),
    ));

    let plan = apply_attachments(
        &config.attachments,
        config.provider,
        &run_dir,
        &socket_path,
        id,
        &mut env,
        &mut args_owned,
        deps.state_machine,
        deps.injection_executor,
        deps.event_router,
        deps.job_projects,
        deps.agent_name,
    )?;

    // Agent-name attribution env. Pushed whenever this spawn
    // carries an agent context (matches `spawn_agent` today —
    // shell jobs leave it unset).
    if has_agent_context {
        env.push(("ORKIA_AGENT_NAME".into(), deps.agent_name.into()));
    }
    // V2 protocol capability env when OSC 133 is wired. Plain
    // shell jobs and non-OSC agent jobs don't get these — they
    // get a baseline `TERM` instead, matching `spawn_shell`.
    if plan.osc133 {
        env.push(("ORKIA".into(), "1".into()));
        env.push(("ORKIA_PROTOCOL_VERSION".into(), "1".into()));
        env.push(("TERM_PROGRAM".into(), "orkia".into()));
    } else {
        env.push(("TERM".into(), "xterm-256color".into()));
    }

    let (on_osc133, on_apc) = if plan.osc133 {
        build_osc133_callbacks(id, deps.agent_name, deps.event_router)
    } else {
        (None, None)
    };

    // launcher. This runs AFTER `apply_attachments`, so `args_owned` already
    // holds the agent's final argv (including provider-specific flags like
    // claude's `--mcp-config`) — the cage wraps the complete command. Only
    // the exec'd process changes; the job's identity (`kind` / `label` /
    // agent attribution, built from `config.command`) is left intact.
    let (exec_cmd, exec_args) =
        cage_wrap_argv(config.cage_wrapper.as_ref(), config.command, args_owned);

    let engine_config = EngineConfig {
        init_cols: cols,
        init_rows: rows,
        cmd: Some(exec_cmd),
        args: exec_args,
        env,
        cwd: working_dir,
        on_osc133,
        on_apc,
        // Agent jobs host a persistent full-screen TUI (claude / codex /
        // gemini) that never emits OSC-133 command markers. Mark the
        // engine so its grid stays live and a (re-)attach can rebuild
        // the screen from `render_visible_snapshot`. Shell jobs leave
        // this off — they run discrete OSC-133-bracketed commands.
        persistent_program: has_agent_context,
        ..EngineConfig::default()
    };

    let engine =
        TerminalEngine::start(engine_config).map_err(|e| ShellError::Other(e.to_string()))?;
    let pid = engine.child_id();

    let kind = if has_agent_context {
        JobKind::Agent {
            agent_id: uuid::Uuid::new_v4(),
            agent_name: deps.agent_name.to_string(),
        }
    } else {
        JobKind::Shell {
            cmd: config.command.to_string(),
        }
    };

    let _ = controller.event_tx().send(JobEvent::Spawned {
        id,
        kind: kind.clone(),
        pid,
    });

    controller.push_entry(JobEntry {
        id,
        kind,
        state: JobState::Running,
        engine,
        started_at: std::time::Instant::now(),
        label,
        lifecycle_hooks: plan.hooks.clone(),
        sink_recipe: None,
    });

    // Fire `on_spawn` on every hook in insertion order. SEAL
    // before StateMachine before InjectionExecutor — the canonical
    // attachment order (see `apply_attachments`) ensures the
    // agent.spawn event lands before any downstream event can race it.
    dispatch_on_spawn(controller, id, deps.agent_name, pid, &plan);

    // Initial bytes stdin: write the bytes to the freshly-spawned
    // PTY master. Non-fatal — a short race between exec and stdin
    // being readable can swallow the first bytes for some agents.
    if let StdinSource::InitialBytes(bytes) = &stdin_source
        && !bytes.is_empty()
        && let Some(entry) = controller.last_entry()
    {
        let mut buf = bytes.clone();
        if !buf.ends_with(b"\n") {
            buf.push(b'\n');
        }
        if let Err(e) = entry.write_stdin(&buf) {
            tracing::warn!("job {}: initial bytes write failed: {e}", id.0);
        }
    }

    Ok(SpawnResult {
        job_id: id,
        system_prompt_hash: plan.system_prompt_hash,
        memory_hash: plan.memory_hash,
        tools_count: plan.tools_count,
    })
}

/// applying the Orkia Cage wrapper when present. Pure and testable: with no
/// wrapper it returns `(command, args)` unchanged (default path); with one it
/// returns `(cage_bin, ["--policy", <path>, "--", command, args…])` so the
/// agent's final argv (incl. provider flags appended upstream) sits intact
/// after the `--` boundary, never between the launcher and `--`.
fn cage_wrap_argv(
    wrapper: Option<&CageWrapper>,
    command: &str,
    mut args: Vec<String>,
) -> (String, Vec<String>) {
    match wrapper {
        Some(w) => {
            let mut wrapped = Vec::with_capacity(args.len() + 4);
            wrapped.push("--policy".to_string());
            wrapped.push(w.policy_path.clone());
            wrapped.push("--".to_string());
            wrapped.push(command.to_string());
            wrapped.append(&mut args);
            (w.cage_bin.clone(), wrapped)
        }
        None => (command.to_string(), args),
    }
}

#[allow(clippy::too_many_arguments)] // Builds an internal plan from a fixed list of subsystem handles; bundling into a struct would just shuffle the same fields around.
fn apply_attachments(
    attachments: &[Attachment],
    provider: orkia_shell_types::ProviderId,
    run_dir: &std::path::Path,
    socket_path: &std::path::Path,
    job_id: JobId,
    env: &mut Vec<(String, String)>,
    args: &mut Vec<String>,
    state_machine: &crate::terminal_state::TerminalStateMachine,
    injection_executor: &crate::injection_executor::InjectionExecutor,
    event_router: &EventRouter,
    job_projects: &Arc<parking_lot::RwLock<std::collections::HashMap<JobId, String>>>,
    agent_name: &str,
) -> Result<AttachmentPlan, ShellError> {
    let mut hooks: Vec<Arc<dyn JobLifecycleHook>> = Vec::new();
    let mut osc133 = false;
    let mut system_prompt_hash = String::new();
    let mut memory_hash = String::new();
    let mut tools_count: usize = 0;
    let mut state_machine_pending_body: Option<String> = None;
    let mut seal_project: Option<String> = None;
    let mut want_state_machine = false;
    let mut want_injection_executor = false;
    let mut want_seal_chain = false;

    for attachment in attachments {
        match attachment {
            Attachment::Hooks { mediate } => {
                install_hooks_for_provider(provider, *mediate);
            }
            Attachment::AgentContext { context } => {
                // `inject_agent_context` writes context.md /
                // mcp-config.json into the run dir and pushes
                // `ORKIA_AGENT_CONTEXT` (and provider-specific
                // env/args via the spawn plan) into the spawn env.
                let (sp, mem, n) = inject_agent_context(
                    Some(context),
                    provider,
                    run_dir,
                    socket_path,
                    job_id,
                    env,
                    args,
                )?;
                system_prompt_hash = sp;
                memory_hash = mem;
                tools_count = n;
            }
            Attachment::Osc133Listener => {
                osc133 = true;
            }
            Attachment::SealChain { project } => {
                want_seal_chain = true;
                seal_project = project.clone();
            }
            Attachment::StateMachine { pending_body } => {
                want_state_machine = true;
                state_machine_pending_body = pending_body.clone();
            }
            Attachment::PendingPrompt => {
                // The state-machine attachment already owns the
                // queue; an explicit `PendingPrompt` without a
                // `StateMachine` is reserved for a future shell-
                // job tell-style path. No-op today.
            }
            Attachment::InjectionExecutor => {
                want_injection_executor = true;
            }
        }
    }

    // table. SEAL hook fires `agent.spawn` first; state machine
    // and injection executor are downstream consumers.
    if want_seal_chain {
        hooks.push(Arc::new(SealChainLifecycle::new(
            event_router.clone(),
            seal_project,
            Arc::clone(job_projects),
            agent_name.to_string(),
        )));
    }
    if osc133 {
        hooks.push(Arc::new(Osc133Lifecycle));
    }
    if want_state_machine {
        hooks.push(Arc::new(StateMachineLifecycle::new(
            state_machine.clone(),
            state_machine_pending_body,
        )));
    }
    if want_injection_executor {
        hooks.push(Arc::new(InjectionExecutorLifecycle::new(
            injection_executor.clone(),
            provider,
        )));
    }

    Ok(AttachmentPlan {
        hooks,
        osc133,
        system_prompt_hash,
        memory_hash,
        tools_count,
    })
}

fn build_osc133_callbacks(
    job_id: JobId,
    agent_name: &str,
    event_router: &EventRouter,
) -> (
    Option<orkia_terminal_core::Osc133Callback>,
    Option<orkia_terminal_core::ApcCallback>,
) {
    let on_osc133 = {
        let router = event_router.clone();
        let agent_name = agent_name.to_string();
        Some(Arc::new(move |marker: orkia_terminal_core::Osc133Marker| {
            router.on_osc133(job_id, &agent_name, marker);
        }) as orkia_terminal_core::Osc133Callback)
    };
    let on_apc = {
        let router = event_router.clone();
        let agent_name = agent_name.to_string();
        Some(Arc::new(move |payload: &[u8]| {
            if let Some(event) = crate::protocol::parse_orkia_apc(payload) {
                router.on_orkia_protocol(job_id, &agent_name, event);
            }
        }) as orkia_terminal_core::ApcCallback)
    };
    (on_osc133, on_apc)
}

fn dispatch_on_spawn(
    controller: &super::JobController,
    job_id: JobId,
    agent_name: &str,
    pid: Option<u32>,
    plan: &AttachmentPlan,
) {
    let Some(entry) = controller.get(job_id) else {
        return;
    };
    let ctx = SpawnContext {
        job_id,
        agent_name,
        pid,
        engine: &entry.engine,
        system_prompt_hash: &plan.system_prompt_hash,
        memory_hash: &plan.memory_hash,
        tools_count: plan.tools_count,
    };
    for hook in &plan.hooks {
        hook.on_spawn(&ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_shell_types::ProcessGroupMode;

    fn shell_argv() -> Vec<String> {
        vec!["-c".into(), "cat".into()]
    }

    #[test]
    fn cage_wrap_none_is_identity() {
        let args = vec!["--mcp-config".to_string(), "x.json".to_string()];
        let (cmd, out) = cage_wrap_argv(None, "claude", args.clone());
        assert_eq!(cmd, "claude");
        assert_eq!(out, args);
    }

    #[test]
    fn cage_wrap_prefixes_launcher_and_keeps_agent_argv_after_separator() {
        let w = CageWrapper {
            cage_bin: "orkia-cage".into(),
            policy_path: "/p/policy.toml".into(),
        };
        // The agent's final argv already includes claude's `--mcp-config`.
        let args = vec!["--mcp-config".to_string(), "x.json".to_string()];
        let (cmd, out) = cage_wrap_argv(Some(&w), "claude", args);
        assert_eq!(cmd, "orkia-cage");
        // `--mcp-config` must land AFTER `claude`, never between the launcher
        assert_eq!(
            out,
            vec![
                "--policy",
                "/p/policy.toml",
                "--",
                "claude",
                "--mcp-config",
                "x.json"
            ]
        );
    }

    #[test]
    fn spawn_minimal_shell_produces_running_job() {
        // Smallest possible spawn: no attachments, no agent
        // context. Verifies the unified path can stand up a PTY
        // and a JobEntry without any of the agent baggage.
        let (mut controller, _rx) = crate::job::JobController::new();
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let approvals = ApprovalWatcher::new(tmp.path());
        let router = EventRouter::new();
        let sm = crate::terminal_state::TerminalStateMachine::new();
        let exec = crate::injection_executor::InjectionExecutor::spawn();
        let projects = Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new()));

        let args = shell_argv();
        let config = JobConfig {
            command: "/bin/sh",
            provider: orkia_shell_types::ProviderId::Generic,
            args: &args,
            label: "test:shell".into(),
            env: Vec::new(),
            working_dir: None,
            stdin: StdinSource::Pty,
            process_group: ProcessGroupMode::NewSession,
            attachments: Vec::new(),
            cage_wrapper: None,
        };
        let deps = SpawnDeps {
            approvals: &approvals,
            event_router: &router,
            state_machine: &sm,
            injection_executor: &exec,
            job_projects: &projects,
            agent_name: "",
        };
        let result = spawn(&mut controller, config, deps).expect("spawn");
        assert!(result.system_prompt_hash.is_empty());
        assert!(result.memory_hash.is_empty());
        assert_eq!(result.tools_count, 0);
        let entry = controller.get(result.job_id).expect("entry present");
        assert!(matches!(entry.kind, JobKind::Shell { .. }));
    }

    #[test]
    fn spawn_with_state_machine_attachment_registers_detector() {
        // Attaching `StateMachine` should make the detector pick
        // up this job — `pending_count` returns 0 (no body) but
        // the entry exists in the queue, which we verify by
        // appending a follow-up body and observing the count
        // increment.
        let (mut controller, _rx) = crate::job::JobController::new();
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let approvals = ApprovalWatcher::new(tmp.path());
        let router = EventRouter::new();
        let sm = crate::terminal_state::TerminalStateMachine::new();
        let exec = crate::injection_executor::InjectionExecutor::spawn();
        let projects = Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new()));

        let args = shell_argv();
        let config = JobConfig {
            command: "/bin/sh",
            provider: orkia_shell_types::ProviderId::Generic,
            args: &args,
            label: "test:sm".into(),
            env: Vec::new(),
            working_dir: None,
            stdin: StdinSource::Pty,
            process_group: ProcessGroupMode::NewSession,
            attachments: vec![Attachment::StateMachine { pending_body: None }],
            cage_wrapper: None,
        };
        let deps = SpawnDeps {
            approvals: &approvals,
            event_router: &router,
            state_machine: &sm,
            injection_executor: &exec,
            job_projects: &projects,
            agent_name: "test-agent",
        };
        let result = spawn(&mut controller, config, deps).expect("spawn");
        assert!(sm.append_body(result.job_id, "hello".into()));
        assert_eq!(sm.pending_count(result.job_id), 1);
    }
}

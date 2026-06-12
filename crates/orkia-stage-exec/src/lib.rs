// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Generic interactive PTY stage executor.
//!
//! One [`StageExecutor`] runs a single pipeline stage from a fully
//! resolved [`StagePlan`]: it creates the stage's run directory, writes
//! the per-stage MCP config, spawns the agent over a real interactive
//! PTY (never print mode), and captures the first output
//! the stage produces. It returns raw bytes; it understands nothing
//! about *which* stages run, in what order, or why — those decisions
//! belong to the kernel brain (this crate is the sole
//! owner of [`TerminalEngine`]; the kernel never touches a PTY).
//!
//! The capture race is resolved in [`collect::await_output`]: the
//! `Stop`-hook → final-response channel (primary — the same turn-end
//! capture Solo dispatch uses) wakes the executor while the agent stays
//! alive; the MCP `PipelineOutput` envelope is a redundant safety net
//! (routed through this executor's [`JournalEnvelopeHook`]); the child
//! exiting (recover on-disk content) and the stage timeout are the
//! final fallbacks.

mod collect;
mod dir;
mod interactive;
mod native_stage;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use orkia_shell::providers::{SpawnPlanInputs, build_spawn_plan};
use orkia_shell_types::{
    FinalResponseSource, JournalEnvelope, JournalEnvelopeHook, ProviderId, StagePlan,
};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use collect::{PipelineOutputPayload, StageProc, await_output};
use dir::PreparedStage;
use interactive::{StageAgentDriver, StageSpawn};

/// Appended to a stage agent's system prompt (claude). It does NOT drive
/// capture — the `Stop`-hook → final-response channel does that whether or
/// not the agent cooperates. It only *arms the safety net*: an agent that
/// reads it can call `submit_pipeline_output` to deliver its result early
/// via the `orkia-pipe` MCP server.
const PIPELINE_PROTOCOL_ADDENDUM: &str = "\n\n---\n# Pipeline\n\
You are running as one stage of an Orkia agent pipeline. Your reply for \
this turn is the stage's output and is forwarded to the next stage \
automatically. When your final answer is ready you may also call the \
`submit_pipeline_output` tool with that answer to deliver it immediately.\n";

/// Build the env + argv a stage agent spawns with, applying Solo parity
/// via the shared [`build_spawn_plan`]: every provider gets the pipeline
/// ids and `ORKIA_AGENT_CONTEXT`; claude additionally gets its system
/// prompt via `CLAUDE_SYSTEM_PROMPT` (assembled context + the pipeline
/// addendum) and loads the merged servers via `--mcp-config <path>`.
/// `gemini_mcp_servers` is the project-settings payload the caller
/// must merge into the stage run dir before spawning, when any.
struct StageSpawnInputs {
    env: Vec<(String, String)>,
    args: Vec<String>,
    gemini_mcp_servers: Option<serde_json::Map<String, serde_json::Value>>,
}

fn spawn_inputs(plan: &StagePlan, prepared: &PreparedStage) -> StageSpawnInputs {
    let mut env = dir::stage_env(plan);
    // Identity comes from the command basename. The plan's `provider`
    // field is the hook-bridge source hint, not a runtime id — it must
    // not override what the spawned binary actually is.
    let provider = ProviderId::from_command(&plan.command);
    let system_prompt = prepared.assembled.as_ref().map(|assembled| {
        let mut s = assembled.clone();
        s.push_str(PIPELINE_PROTOCOL_ADDENDUM);
        s
    });
    let spawn_plan = build_spawn_plan(SpawnPlanInputs {
        provider,
        assembled_system_prompt: system_prompt.as_deref(),
        context_path: prepared.context_path.as_deref(),
        mcp_config_path: Some(&prepared.mcp_config_path),
        mcp_servers: Some(&prepared.mcp_servers),
        mediate_requested: false,
    });
    env.extend(spawn_plan.env);
    let mut args = plan.args.clone();
    args.extend(spawn_plan.args);
    StageSpawnInputs {
        env,
        args,
        gemini_mcp_servers: spawn_plan.gemini_mcp_servers,
    }
}

/// Shell-owned configuration every stage execution needs. None of these
/// paths come from the kernel plan — the shell owns its own filesystem
/// and socket layout, the kernel only names ids.
#[derive(Clone)]
pub struct StageExecConfig {
    /// Root of the shell's data dir; stage run dirs hang off
    /// `<data_dir>/pipelines/<pipeline_id>/stage-<index>/`.
    pub data_dir: PathBuf,
    /// The shell's journal socket the per-stage MCP pipe server connects
    /// back to.
    pub socket_path: PathBuf,
    /// Primary capture channel and fallback transcript source: the
    /// `Stop`-hook → final-response service the REPL also owns.
    pub final_response_source: Arc<dyn FinalResponseSource>,
    /// Assembles each stage agent's context (system prompt + tools), the
    /// same way Solo dispatch does — so a stage `@faye` is the real faye.
    pub context_provider: Arc<dyn StageContextProvider>,
    /// Timeout applied when a [`StagePlan`] carries no `timeout_secs`.
    pub default_stage_timeout: Duration,
}

/// Assembles a stage agent's [`AgentContext`](orkia_shell::agent_context::AgentContext)
/// from its name, mirroring the REPL's Solo `build_agent_context`. The
/// impl lives in `bins/orkia` (it reads the agent registry + workspace);
/// keeping it behind a trait keeps this crate free of shell wiring.
pub trait StageContextProvider: Send + Sync {
    /// `None` when the agent is unknown: the stage still spawns, just
    /// without identity/tools. Capture is provider-hook driven and
    /// unaffected (fail-closed: log, never silently premium).
    fn context_for(&self, agent: &str) -> Option<orkia_shell::agent_context::AgentContext>;
}

/// The bytes one stage produced, plus how they were captured and the
/// canonical file the executor wrote them to.
#[derive(Clone, Debug)]
pub struct StageOutput {
    /// The raw output bytes — handed back to the kernel verbatim, never
    /// interpreted here.
    pub bytes: Vec<u8>,
    /// `true` when delivered through the MCP `PipelineOutput` channel,
    /// `false` when recovered from the final-response fallback.
    pub via_mcp: bool,
    /// Wall-clock spent in this stage, milliseconds.
    pub elapsed_ms: u64,
    /// The file the capturing channel already wrote: the final-response
    /// `response_path` (primary), the MCP `pipeline-output.md` (safety
    /// net), or the transcript fallback. No `carry.bin` copy — the kernel
    /// reads this path directly when composing the next stage. This is
    /// what fills [`StageOutputRef::path`].
    pub output_path: PathBuf,
}

/// Map from `(pipeline_id, stage_index)` to the channel awaiting that
/// stage's MCP output. Shared between [`StageExecutor::run_stage`] (which
/// registers a waiter before spawning) and the [`JournalEnvelopeHook`]
/// impl (which routes a matching envelope to it).
type Waiters = Mutex<HashMap<(String, u32), UnboundedSender<PipelineOutputPayload>>>;

/// Runs pipeline stages over real interactive PTYs. Cheap to clone — the
/// driver wraps channel senders and the waiter map is shared.
#[derive(Clone)]
pub struct StageExecutor {
    driver: StageAgentDriver,
    config: StageExecConfig,
    waiters: Arc<Waiters>,
}

impl StageExecutor {
    /// Stand up the shared injection machinery (one typist + detector +
    /// bridging worker) and hold the shell-owned config.
    pub fn new(config: StageExecConfig) -> Self {
        Self {
            driver: StageAgentDriver::new(),
            config,
            waiters: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Execute one resolved stage end to end: prepare its run dir + MCP
    /// config, register the output waiter, spawn the agent interactively,
    /// and block (async) on the first of MCP-output / child-exit /
    /// timeout. The waiter is always removed before returning.
    pub async fn run_stage(&self, plan: &StagePlan) -> Result<StageOutput, String> {
        let run_dir = dir::create_run_dir(&self.config, plan)?;
        // Assemble the agent's identity + tools the way Solo dispatch does,
        // then write its `context.md` + merged `mcp-config.json` (agent
        // servers + the `orkia-pipe` safety net). Absent a context the
        // stage still spawns and captures via the `Stop` hook.
        let context = self.config.context_provider.context_for(&plan.agent);
        let prepared = dir::prepare_artifacts(&self.config, plan, &run_dir, context.as_ref())
            .map_err(|e| format!("prepare stage artifacts: {e}"))?;
        // Install the provider hooks so the `Stop` hook fires at turn-end
        // and the final-response channel (primary capture) arms.
        dir::install_stage_hooks(plan, &run_dir);

        let key = (plan.pipeline_id.clone(), plan.stage_index);
        let (tx, rx) = unbounded_channel();
        self.register_waiter(key.clone(), tx);

        let result = self.spawn_and_collect(plan, run_dir, prepared, rx).await;
        self.remove_waiter(&key);
        result
    }

    /// Execute one native stage (`plan.runtime` is `Some(model_ref)`):
    /// one bounded turn of the Orkia-owned LLM loop instead of a PTY
    /// agent — see [`native_stage`]. No waiter: the turn's final text
    /// is the output, there is no capture race to arbitrate.
    pub async fn run_native_stage(
        &self,
        plan: &StagePlan,
        kernel: &Arc<dyn orkia_shell_types::KernelRpc>,
    ) -> Result<StageOutput, String> {
        native_stage::run(&self.config, plan, kernel).await
    }

    /// Spawn the stage agent and await its output. Split out of
    /// [`Self::run_stage`] so the waiter cleanup there covers every exit
    /// path of this fallible body.
    async fn spawn_and_collect(
        &self,
        plan: &StagePlan,
        run_dir: PathBuf,
        prepared: PreparedStage,
        rx: tokio::sync::mpsc::UnboundedReceiver<PipelineOutputPayload>,
    ) -> Result<StageOutput, String> {
        let inputs = spawn_inputs(plan, &prepared);
        if let Some(servers) = &inputs.gemini_mcp_servers {
            dir::merge_stage_gemini_mcp(&run_dir, servers);
        }
        let (env, args) = (inputs.env, inputs.args);
        let composed = plan.composed_body.clone().into_bytes();
        let spawn = StageSpawn {
            command: &plan.command,
            args: &args,
            composed: &composed,
            run_dir: &run_dir,
            env,
            job_id: plan.job_id,
            agent: &plan.agent,
        };
        let engine = self.driver.spawn_stage(spawn)?;

        let timeout = plan
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(self.config.default_stage_timeout);
        let mut proc = StageProc {
            engine,
            rx,
            run_dir,
            job_id: plan.job_id,
            stage_index: plan.stage_index,
            agent: plan.agent.clone(),
            started: Instant::now(),
            timeout,
        };
        await_output(&self.config, &self.driver, &mut proc).await
    }

    fn register_waiter(&self, key: (String, u32), tx: UnboundedSender<PipelineOutputPayload>) {
        if let Ok(mut waiters) = self.waiters.lock() {
            waiters.insert(key, tx);
        }
    }

    fn remove_waiter(&self, key: &(String, u32)) {
        if let Ok(mut waiters) = self.waiters.lock() {
            waiters.remove(key);
        }
    }
}

impl JournalEnvelopeHook for StageExecutor {
    /// Route a `PipelineOutput` envelope to the stage waiting on it. The
    /// MCP pipe server writes the stage's output to disk and emits this
    /// envelope; we read the file (bounded by the MCP server's 8 MiB cap)
    /// and hand the bytes to the matching channel. Non-blocking send;
    /// runs off the REPL loop on the journal listener.
    fn on_envelope(&self, env: &JournalEnvelope) {
        if env.event.as_deref() != Some("PipelineOutput") {
            return;
        }
        let (Some(pipeline_id), Some(stage_index), Some(path)) = (
            env.pipeline_id.as_deref(),
            env.stage_index,
            env.response_path.as_ref(),
        ) else {
            return;
        };
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => return,
        };
        let payload = PipelineOutputPayload {
            bytes,
            via_mcp: true,
            path: PathBuf::from(path),
        };
        if let Ok(waiters) = self.waiters.lock()
            && let Some(tx) = waiters.get(&(pipeline_id.to_string(), stage_index))
        {
            let _ = tx.send(payload);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_shell_types::{EventType, FinalResponseEvent};

    struct NoFinalResponse;
    impl FinalResponseSource for NoFinalResponse {
        fn subscribe(&self, _cb: orkia_shell_types::FinalResponseCallback) {}
        fn latest_for_job(&self, _job_id: u32) -> Option<FinalResponseEvent> {
            None
        }
    }

    struct NoContext;
    impl StageContextProvider for NoContext {
        fn context_for(&self, _agent: &str) -> Option<orkia_shell::agent_context::AgentContext> {
            None
        }
    }

    fn test_config() -> StageExecConfig {
        StageExecConfig {
            data_dir: PathBuf::from("/data"),
            socket_path: PathBuf::from("/tmp/orkia.sock"),
            final_response_source: Arc::new(NoFinalResponse),
            context_provider: Arc::new(NoContext),
            default_stage_timeout: Duration::from_secs(120),
        }
    }

    fn pipeline_output_env(pipeline_id: &str, stage_index: u32, path: &str) -> JournalEnvelope {
        JournalEnvelope {
            event_type: EventType::Hook,
            event: Some("PipelineOutput".to_string()),
            pipeline_id: Some(pipeline_id.to_string()),
            stage_index: Some(stage_index),
            response_path: Some(path.to_string()),
            ..JournalEnvelope::default()
        }
    }

    fn claude_plan() -> StagePlan {
        StagePlan {
            pipeline_id: "p".into(),
            stage_index: 0,
            job_id: 5,
            agent: "sage".into(),
            command: "claude".into(),
            args: vec!["--resume".into()],
            provider: Some("anthropic".into()),
            runtime: None,
            composed_body: "go".into(),
            timeout_secs: None,
        }
    }

    #[test]
    fn spawn_inputs_adds_mcp_config_and_system_prompt_for_claude() {
        let prepared = PreparedStage {
            assembled: Some("you are sage".into()),
            context_path: Some(PathBuf::from("/run/context.md")),
            mcp_config_path: PathBuf::from("/run/mcp-config.json"),
            mcp_servers: serde_json::Map::new(),
        };
        let StageSpawnInputs { env, args, .. } = spawn_inputs(&claude_plan(), &prepared);

        // `--mcp-config <path>` appended after the plan's own args.
        let pos = args.iter().position(|a| a == "--mcp-config").expect("flag");
        assert_eq!(args[pos + 1], "/run/mcp-config.json");
        assert_eq!(args.first().map(String::as_str), Some("--resume"));

        let map: HashMap<_, _> = env.into_iter().collect();
        assert_eq!(map["ORKIA_AGENT_CONTEXT"], "/run/context.md");
        let sys = &map["CLAUDE_SYSTEM_PROMPT"];
        assert!(sys.starts_with("you are sage"));
        // The protocol addendum (MCP safety net) is appended to the prompt.
        assert!(sys.contains("submit_pipeline_output"));
    }

    #[test]
    fn spawn_inputs_skips_claude_only_wiring_for_other_providers() {
        let mut plan = claude_plan();
        plan.command = "codex".into();
        let prepared = PreparedStage {
            assembled: Some("ignored".into()),
            context_path: None,
            mcp_config_path: PathBuf::from("/run/mcp-config.json"),
            mcp_servers: serde_json::Map::new(),
        };
        let StageSpawnInputs { env, args, .. } = spawn_inputs(&plan, &prepared);
        assert!(!args.iter().any(|a| a == "--mcp-config"));
        assert!(!env.iter().any(|(k, _)| k == "CLAUDE_SYSTEM_PROMPT"));
    }

    #[test]
    fn on_envelope_routes_matching_output_to_waiter() {
        let exec = StageExecutor::new(test_config());
        let (tx, mut rx) = unbounded_channel();
        exec.register_waiter(("pipe-1".to_string(), 0), tx);

        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("pipeline-output.md");
        std::fs::write(&out, b"stage-0 output").expect("write");

        exec.on_envelope(&pipeline_output_env("pipe-1", 0, &out.to_string_lossy()));

        let payload = rx.try_recv().expect("payload delivered");
        assert_eq!(payload.bytes, b"stage-0 output");
        assert!(payload.via_mcp);
    }

    #[test]
    fn on_envelope_ignores_non_pipeline_events() {
        let exec = StageExecutor::new(test_config());
        let (tx, mut rx) = unbounded_channel();
        exec.register_waiter(("pipe-1".to_string(), 0), tx);

        let mut env = pipeline_output_env("pipe-1", 0, "/nonexistent");
        env.event = Some("Stop".to_string());
        exec.on_envelope(&env);

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn on_envelope_ignores_unregistered_stage() {
        let exec = StageExecutor::new(test_config());
        let (tx, mut rx) = unbounded_channel();
        exec.register_waiter(("pipe-1".to_string(), 0), tx);

        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("out.md");
        std::fs::write(&out, b"x").expect("write");

        // Different stage index — no waiter for it.
        exec.on_envelope(&pipeline_output_env("pipe-1", 5, &out.to_string_lossy()));
        assert!(rx.try_recv().is_err());
    }
}

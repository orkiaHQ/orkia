// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! On-disk artifacts for a stage: the run directory, the per-stage MCP
//! config the agent loads, and the environment it inherits. All paths
//! derive from the shell's own [`crate::StageExecConfig`] — the kernel
//! plan never names the shell's filesystem or socket layout.

use std::path::{Path, PathBuf};

use orkia_shell::agent_context::{self, AgentContext};
use orkia_shell_types::StagePlan;

use crate::StageExecConfig;

/// The on-disk artifacts a prepared stage exposes to the spawn step: the
/// assembled system prompt (claude only) and the path to the merged
/// `mcp-config.json` the agent loads via `--mcp-config`.
pub(crate) struct PreparedStage {
    /// `Some` when a context was assembled — becomes `CLAUDE_SYSTEM_PROMPT`.
    pub(crate) assembled: Option<String>,
    /// Path to the agent's `context.md`, when written. Exposed as
    /// `ORKIA_AGENT_CONTEXT` so every provider can read its brief.
    pub(crate) context_path: Option<PathBuf>,
    /// Path to the merged (or pipe-only) `mcp-config.json`.
    pub(crate) mcp_config_path: PathBuf,
    /// The merged `mcpServers` object behind `mcp_config_path` — the
    /// spawn plan renders non-Claude MCP delivery from this (Codex `-c`
    /// overrides, Gemini settings merge).
    pub(crate) mcp_servers: serde_json::Map<String, serde_json::Value>,
}

/// Per-stage run directory: `<data_dir>/pipelines/<pipeline_id>/stage-<index>/`.
pub(crate) fn run_dir(config: &StageExecConfig, plan: &StagePlan) -> PathBuf {
    config
        .data_dir
        .join("pipelines")
        .join(&plan.pipeline_id)
        .join(format!("stage-{}", plan.stage_index))
}

/// Create the run directory. Returns its path.
pub(crate) fn create_run_dir(
    config: &StageExecConfig,
    plan: &StagePlan,
) -> Result<PathBuf, String> {
    let dir = run_dir(config, plan);
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir run-dir: {e}"))?;
    Ok(dir)
}

/// Install the stage agent's provider hooks into the run dir (its cwd),
/// the same configuration Solo dispatch writes. This is what makes the
/// `Stop` hook fire `orkia bridge --source <provider>` at turn-end, which
/// drives the final-response capture in [`crate::collect::await_output`].
///
/// Fail-closed visibility: a missing/unknown provider or a write failure
/// is logged, never propagated — the stage still spawns (the capture
/// channel just won't arm). `mediate=false`: stages run off the cage.
pub(crate) fn install_stage_hooks(plan: &StagePlan, run_dir: &Path) {
    let Some(name) = plan.provider.as_deref() else {
        tracing::warn!(
            stage = plan.stage_index,
            agent = %plan.agent,
            "pipeline: stage has no provider; Stop-hook capture will not arm",
        );
        return;
    };
    let provider = orkia_shell_types::ProviderId::parse(name);
    if !provider.capabilities().hooks_capture {
        tracing::warn!(
            stage = plan.stage_index,
            provider = name,
            "pipeline: provider has no hook capture; Stop-hook capture will not arm",
        );
        return;
    }
    match orkia_shell::hooks::install_hooks(run_dir, provider, false) {
        Ok(Some(path)) => tracing::debug!(
            "pipeline: installed {} hooks at {}",
            provider.as_str(),
            path.display()
        ),
        Ok(None) => {}
        Err(e) => tracing::warn!(
            "pipeline: hook install failed for {}: {e}",
            provider.as_str()
        ),
    }
}

/// Build the env an interactive stage agent inherits: the pipeline/stage
/// /job ids, the stage's agent name, and the provider hint. `ORKIA_JOB_ID`
/// is load-bearing — the `Stop` hook's `orkia bridge` reads it to tag the
/// final-response event. `ORKIA_AGENT_NAME` is what attributes a stage's
/// captured turn to `faye`/`rex` instead of falling back to the bare
/// provider source ("claude") — Solo dispatch sets it the same way
/// (`job/spawn.rs`). The MCP config is delivered via `--mcp-config` (added
/// at spawn for claude), not env, matching Solo dispatch.
pub(crate) fn stage_env(plan: &StagePlan) -> Vec<(String, String)> {
    let mut env = vec![
        ("ORKIA_PIPELINE_ID".to_string(), plan.pipeline_id.clone()),
        (
            "ORKIA_STAGE_INDEX".to_string(),
            plan.stage_index.to_string(),
        ),
        ("ORKIA_JOB_ID".to_string(), plan.job_id.to_string()),
        ("ORKIA_AGENT_NAME".to_string(), plan.agent.clone()),
    ];
    if let Some(provider) = &plan.provider {
        env.push(("ORKIA_PROVIDER".to_string(), provider.clone()));
    }
    env
}

/// The `orkia-pipe` MCP server entry: the shell's own binary run as
/// `orkia mcp-pipe` (resolved from the current executable, so it is always
/// co-located). Socket path from config, ids from the plan.
fn orkia_pipe_entry(
    config: &StageExecConfig,
    plan: &StagePlan,
    run_dir: &Path,
) -> std::io::Result<serde_json::Value> {
    let orkia_exe = std::env::current_exe()?;
    Ok(serde_json::json!({
        "command": orkia_exe.to_string_lossy(),
        "args": ["mcp-pipe"],
        "env": {
            "ORKIA_PIPELINE_ID": plan.pipeline_id,
            "ORKIA_STAGE_INDEX": plan.stage_index.to_string(),
            "ORKIA_JOB_ID": plan.job_id.to_string(),
            "ORKIA_AGENT_NAME": plan.agent,
            "ORKIA_RUN_DIR": run_dir.to_string_lossy(),
            "ORKIA_SOCKET_PATH": config.socket_path.to_string_lossy(),
        }
    }))
}

/// Prepare a stage's on-disk artifacts and return what the spawn step
/// needs. When `context` is present, this writes the agent's `context.md`
/// and an `mcp-config.json` that MERGES the agent's own MCP servers (the
/// agent's tools plus the knowledge bridge, via Solo's
/// [`agent_context::write_to_run_dir`]) with the `orkia-pipe` safety-net
/// server. Absent a context, it writes a pipe-only config — the stage
/// still captures via the `Stop` hook.
pub(crate) fn prepare_artifacts(
    config: &StageExecConfig,
    plan: &StagePlan,
    run_dir: &Path,
    context: Option<&AgentContext>,
) -> std::io::Result<PreparedStage> {
    let pipe_entry = orkia_pipe_entry(config, plan, run_dir)?;
    let mcp_path = run_dir.join("mcp-config.json");

    let (assembled, context_path) = match context {
        Some(ctx) => {
            let (ctx_path, _) =
                agent_context::write_to_run_dir(run_dir, ctx, &config.socket_path, plan.job_id)?;
            (Some(ctx.assembled.clone()), Some(ctx_path))
        }
        None => (None, None),
    };

    // Start from the agent's own servers (written by `write_to_run_dir`
    // when a context exists), then splice in `orkia-pipe`. A fresh map
    // when there was no context or no agent servers.
    let mut root = match std::fs::read_to_string(&mcp_path) {
        Ok(raw) => serde_json::from_str::<serde_json::Value>(&raw)
            .unwrap_or_else(|_| serde_json::json!({ "mcpServers": {} })),
        Err(_) => serde_json::json!({ "mcpServers": {} }),
    };
    if !root
        .get("mcpServers")
        .map(|v| v.is_object())
        .unwrap_or(false)
    {
        root["mcpServers"] = serde_json::json!({});
    }
    root["mcpServers"]["orkia-pipe"] = pipe_entry;
    let body = serde_json::to_string_pretty(&root)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&mcp_path, body)?;
    let mcp_servers = root
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    Ok(PreparedStage {
        assembled,
        context_path,
        mcp_config_path: mcp_path,
        mcp_servers,
    })
}

/// Merge the stage's MCP servers into `<run_dir>/.gemini/settings.json`
/// — the stage agent's cwd, where gemini reads project-scope settings
/// (it has no per-invocation config flag; P1.8 mechanisms note). Same
/// fail-closed visibility as [`install_stage_hooks`]: logged, never
/// fatal — the stage still spawns, just without the orkia MCP servers.
pub(crate) fn merge_stage_gemini_mcp(
    run_dir: &Path,
    servers: &serde_json::Map<String, serde_json::Value>,
) {
    let path = run_dir.join(".gemini").join("settings.json");
    if let Err(e) = orkia_shell::hooks::merge_mcp_servers(&path, servers) {
        tracing::warn!("pipeline: gemini mcp settings merge failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use orkia_shell_types::{FinalResponseEvent, FinalResponseSource};

    use crate::StageContextProvider;

    struct NoFinalResponse;
    impl FinalResponseSource for NoFinalResponse {
        fn subscribe(&self, _cb: orkia_shell_types::FinalResponseCallback) {}
        fn latest_for_job(&self, _job_id: u32) -> Option<FinalResponseEvent> {
            None
        }
    }

    struct NoContext;
    impl StageContextProvider for NoContext {
        fn context_for(&self, _agent: &str) -> Option<AgentContext> {
            None
        }
    }

    fn test_config(data_dir: PathBuf) -> StageExecConfig {
        StageExecConfig {
            data_dir,
            socket_path: PathBuf::from("/tmp/orkia.sock"),
            final_response_source: Arc::new(NoFinalResponse),
            context_provider: Arc::new(NoContext),
            default_stage_timeout: Duration::from_secs(120),
        }
    }

    fn test_plan() -> StagePlan {
        StagePlan {
            pipeline_id: "pipe-123".to_string(),
            stage_index: 2,
            job_id: 77,
            agent: "sage".to_string(),
            command: "claude".to_string(),
            args: vec!["--foo".to_string()],
            provider: Some("anthropic".to_string()),
            runtime: None,
            composed_body: "review this".to_string(),
            timeout_secs: None,
        }
    }

    #[test]
    fn run_dir_nests_pipeline_and_stage() {
        let cfg = test_config(PathBuf::from("/data"));
        let plan = test_plan();
        assert_eq!(
            run_dir(&cfg, &plan),
            PathBuf::from("/data/pipelines/pipe-123/stage-2")
        );
    }

    #[test]
    fn create_run_dir_makes_the_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = test_config(tmp.path().to_path_buf());
        let plan = test_plan();
        let dir = create_run_dir(&cfg, &plan).expect("create");
        assert!(dir.is_dir());
        assert!(dir.ends_with("pipelines/pipe-123/stage-2"));
    }

    #[test]
    fn stage_env_carries_ids_and_provider() {
        let plan = test_plan();
        let env: std::collections::HashMap<_, _> = stage_env(&plan).into_iter().collect();
        assert_eq!(env["ORKIA_PIPELINE_ID"], "pipe-123");
        assert_eq!(env["ORKIA_STAGE_INDEX"], "2");
        assert_eq!(env["ORKIA_JOB_ID"], "77");
        assert_eq!(env["ORKIA_AGENT_NAME"], "sage");
        assert_eq!(env["ORKIA_PROVIDER"], "anthropic");
        // MCP config now arrives via `--mcp-config`, not env.
        assert!(!env.contains_key("ORKIA_MCP_CONFIG"));
    }

    #[test]
    fn stage_env_omits_provider_when_absent() {
        let mut plan = test_plan();
        plan.provider = None;
        let keys: Vec<_> = stage_env(&plan).into_iter().map(|(k, _)| k).collect();
        assert!(!keys.iter().any(|k| k == "ORKIA_PROVIDER"));
    }

    #[test]
    fn prepare_artifacts_writes_pipe_only_config_without_context() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = test_config(tmp.path().to_path_buf());
        let plan = test_plan();
        let run = create_run_dir(&cfg, &plan).expect("create");
        let prepared = prepare_artifacts(&cfg, &plan, &run, None).expect("prepare");
        assert!(prepared.assembled.is_none());
        assert!(prepared.context_path.is_none());

        let raw = std::fs::read_to_string(&prepared.mcp_config_path).expect("read back");
        let json: serde_json::Value = serde_json::from_str(&raw).expect("parse");
        let entry = &json["mcpServers"]["orkia-pipe"];
        // command is the running `orkia` binary; args invoke the subcommand.
        assert_eq!(entry["args"][0], "mcp-pipe");
        assert_eq!(entry["env"]["ORKIA_PIPELINE_ID"], "pipe-123");
        assert_eq!(entry["env"]["ORKIA_AGENT_NAME"], "sage");
        assert_eq!(entry["env"]["ORKIA_SOCKET_PATH"], "/tmp/orkia.sock");
    }

    #[test]
    fn prepare_artifacts_merges_agent_servers_with_pipe() {
        use orkia_shell_types::{AgentToolsFile, McpServerEntry};

        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = test_config(tmp.path().to_path_buf());
        let plan = test_plan();
        let run = create_run_dir(&cfg, &plan).expect("create");

        let ctx = AgentContext {
            name: "sage".into(),
            assembled: "you are sage".into(),
            system_prompt: String::new(),
            memory: String::new(),
            tools: AgentToolsFile {
                mcp: vec![McpServerEntry {
                    name: "faye-tool".into(),
                    url: "stdio://faye-mcp".into(),
                    args: vec![],
                    env: Default::default(),
                    description: None,
                }],
                ..Default::default()
            },
            knowledge_mcp_bridge: false,
        };

        let prepared = prepare_artifacts(&cfg, &plan, &run, Some(&ctx)).expect("prepare");
        assert!(prepared.context_path.is_some());

        let raw = std::fs::read_to_string(&prepared.mcp_config_path).expect("read back");
        let json: serde_json::Value = serde_json::from_str(&raw).expect("parse");
        // Both the agent's own server AND the orkia-pipe safety net present.
        assert!(json["mcpServers"]["faye-tool"].is_object());
        assert_eq!(json["mcpServers"]["orkia-pipe"]["args"][0], "mcp-pipe");
        // The structured map handed to the spawn plan mirrors the file.
        assert!(prepared.mcp_servers.contains_key("faye-tool"));
        assert!(prepared.mcp_servers.contains_key("orkia-pipe"));
    }
}

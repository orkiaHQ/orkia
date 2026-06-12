// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Free helpers shared by the job-controller spawn paths.
//!
//! These were extracted out of `job/mod.rs` to keep that module under the
//! 600-line size limit (REF-006). They carry no `JobController` state — each
//! is a standalone function consumed by [`super::spawn`] (and `spawn_shell`).

use std::path::Path;

use orkia_shell_types::ProviderId;

use crate::JobId;
use crate::agent_context::{self, AgentContext};
use crate::error::ShellError;
use crate::hooks;
use crate::providers::{SpawnPlanInputs, build_spawn_plan};

/// Write the filesystem context bundle into the run dir and populate
/// env + args. Returns `(prompt_hash, memory_hash, tools_count)`.
pub(super) fn inject_agent_context(
    context: Option<&AgentContext>,
    provider: ProviderId,
    run_dir: &Path,
    socket_path: &Path,
    job_id: JobId,
    env: &mut Vec<(String, String)>,
    args: &mut Vec<String>,
) -> Result<(String, String, usize), ShellError> {
    let Some(context) = context else {
        return Ok((String::new(), String::new(), 0));
    };
    let (context_path, mcp) =
        agent_context::write_to_run_dir(run_dir, context, socket_path, job_id.0)
            .map_err(|e| ShellError::Other(format!("write agent context: {e}")))?;
    // scalar was authority over no decision and is removed everywhere — the agent
    // no longer sees it. Effective trust is per-(project × capability), in policy.
    let plan = build_spawn_plan(SpawnPlanInputs {
        provider,
        assembled_system_prompt: Some(&context.assembled),
        context_path: Some(&context_path),
        mcp_config_path: mcp.as_ref().map(|m| m.path.as_path()),
        mcp_servers: mcp.as_ref().map(|m| &m.servers),
        mediate_requested: false,
    });
    env.extend(plan.env);
    args.extend(plan.args);
    if let Some(servers) = &plan.gemini_mcp_servers {
        merge_gemini_project_mcp(servers);
    }
    Ok((
        context.system_prompt_hash(),
        context.memory_hash(),
        context.tools_count(),
    ))
}

/// Merge the per-job MCP servers into the project-scope
/// `.gemini/settings.json` under the job's working directory — gemini
/// has no per-invocation config flag, so delivery is a settings merge
/// (P1.8 mechanisms note). Same cwd posture as
/// [`install_hooks_for_provider`]. Logged, never propagated: the spawn
/// proceeds, the agent just won't see the orkia MCP servers.
fn merge_gemini_project_mcp(servers: &serde_json::Map<String, serde_json::Value>) {
    let Ok(cwd) = std::env::current_dir() else {
        tracing::warn!("gemini mcp: cwd unavailable, skipping settings merge");
        return;
    };
    let path = cwd.join(".gemini").join("settings.json");
    if let Err(e) = hooks::merge_mcp_servers(&path, servers) {
        tracing::warn!("gemini mcp: settings merge failed: {e}");
    }
}

/// Install provider hooks into the current working directory.
/// Logs but does not propagate errors — see call site comment.
///
/// `mediate` is true only for a caged spawn: it adds the `orkia-sh hook`
/// Off the cage it is false and the installed config is unchanged from before.
pub(super) fn install_hooks_for_provider(provider: ProviderId, mediate: bool) {
    if !provider.capabilities().hooks_capture {
        return;
    }
    let Ok(cwd) = std::env::current_dir() else {
        tracing::warn!("hooks: cwd unavailable, skipping install");
        return;
    };
    match hooks::install_hooks(&cwd, provider, mediate) {
        Ok(Some(path)) => {
            tracing::debug!(
                "hooks: installed {} config at {}",
                provider.as_str(),
                path.display()
            );
        }
        Ok(None) => {}
        Err(e) => tracing::warn!("hooks: install failed for {}: {e}", provider.as_str()),
    }
}

/// Maximum concurrent background shell jobs. Derived from the
/// process's `RLIMIT_NOFILE` soft limit divided by 4 — each spawn
/// opens roughly three fds (PTY master + slave + per-job log) and
/// the REPL needs headroom for stdin/stdout, agent PTYs, the
/// journal socket, and ambient libraries. `None` when the limit
/// can't be queried; callers treat that as "no cap".
pub(super) fn bg_job_cap() -> Option<usize> {
    let mut rl = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `rl` is a valid `libc::rlimit` on the stack;
    // `getrlimit` is documented thread-safe and writes through the
    // pointer we hand it. Returns 0 on success, -1 on failure.
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) };
    if rc != 0 {
        return None;
    }
    // `rlim_cur` can be `RLIM_INFINITY` on some platforms (a very
    // large sentinel value). The divide can't overflow because
    // `rlim_cur` is at most u64::MAX; `as usize` saturates on 32-
    // bit targets which would mean "no practical cap" — fine.
    let cap = rl.rlim_cur / 4;
    if cap == 0 { None } else { Some(cap as usize) }
}

pub(super) fn terminal_dims() -> (usize, usize) {
    terminal_size::terminal_size()
        .map(|(w, h)| (w.0 as usize, h.0 as usize))
        .unwrap_or((120, 42))
}

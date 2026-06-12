// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

//! "job-id authority").
//!
//! Detached agents survive a REPL exit because the `pty_daemon` owns their
//! runtime, not the in-process `JobController`. Without a bridge the bare-word
//! `ps` / `wait` / `kill` / `tell` builtins only see REPL-local jobs and report
//! "no job matching '1'" for a job the daemon is happily running. This impl of
//! [`DaemonJobs`] lets the main REPL fold the daemon's roster into those builtins
//! over `client_api`.
//!
//! Installed ONLY on the main REPL: [`provider`] returns `None` when this process
//! is itself a detached runtime (`ORKIA_DETACHED_JOB_ID` set), so a runtime never
//! recurses into its own daemon.

use std::sync::Arc;
use std::time::Duration;

use orkia_shell::ShellConfig;
use orkia_shell_types::{DaemonJobView, DaemonJobs, DaemonStageView};

struct DaemonJobsBridge {
    config: ShellConfig,
}

impl DaemonJobs for DaemonJobsBridge {
    fn list(&self) -> Vec<DaemonJobView> {
        super::client_api::list(&self.config)
            .into_iter()
            .map(|j| DaemonJobView {
                id: j.id,
                agent: j.agent,
                state: j.state,
                pid: j.pid,
                label: j.label,
                runtime_secs: j.runtime_secs,
                exit_code: j.exit_code,
                stages: j
                    .stages
                    .into_iter()
                    .map(|s| DaemonStageView {
                        id: s.id,
                        target: s.target,
                        state: s.state,
                        pid: s.pid,
                        runtime_secs: s.runtime_secs,
                        exit_code: s.exit_code,
                        attachable: s.attachable,
                    })
                    .collect(),
            })
            .collect()
    }

    fn wait(&self, id: u32, timeout: Duration) -> Result<(String, i32), String> {
        let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
        let job = super::client_api::wait(&self.config, id, timeout_ms)?;
        // Same convention as the CLI `wait` verb (daemon_cli.rs): a job
        // that resolved without a recorded code waited successfully → 0.
        Ok((
            format!("[{}] {}", job.id, job.state),
            job.exit_code.unwrap_or(0),
        ))
    }

    fn kill(&self, id: u32) -> Result<(), String> {
        super::client_api::kill(&self.config, id)
    }

    fn tell(&self, id: u32, message: &str) -> Result<(), String> {
        // The daemon routes `tell` to a specific agent/stage within the job
        // (`job_accepts_target`); for a single detached agent that target is the
        // job's agent name, `@`-stripped and taken from the first `|` segment.
        let target = super::client_api::list(&self.config)
            .into_iter()
            .find(|j| j.id == id)
            .map(|j| {
                j.agent
                    .split('|')
                    .next()
                    .unwrap_or(j.agent.as_str())
                    .trim_start_matches('@')
                    .to_string()
            })
            .ok_or_else(|| format!("no such job: {id}"))?;
        super::client_api::tell(&self.config, id, &target, message)
    }

    fn attach(&self, id: u32) -> Result<(), String> {
        // Runtime PTY bridge: the PTY master the daemon holds is the runtime
        // WRAPPER's, not the nested agent's (claude runs on a second PTY
        // inside the runtime) — a job-level splice reaches the wrong process
        // and keystrokes never hit the agent. For an agent job, resolve the
        // stage target (same first-`|`-segment rule as `tell`) so the daemon
        // routes through the per-job runtime control socket to the REAL agent
        // PTY (`handle_attach` → `runtime_control::attach_proxy` → the
        // runtime's stage-attach pump). Non-agent `-c` jobs keep the
        // job-level splice — the wrapper PTY IS the real surface there.
        // Raw mode stays the caller's responsibility (REPL `RawModeGuard`).
        // Attach is always called with an id the resolver JUST saw in the
        // roster; the only way it is gone one list later is the daemon's
        // done-reap (finished jobs are reported once, then leave the
        // roster). Say that, not "no such job".
        let job = super::client_api::list(&self.config)
            .into_iter()
            .find(|j| j.id == id)
            .ok_or_else(|| format!("job {id} already finished; nothing to attach"))?;
        // A finished job has no PTY to splice — say so plainly instead of
        // surfacing the daemon's "no longer owns its PTY" lost-PTY error.
        if matches!(job.state.as_str(), "done" | "stopped" | "failed") {
            return Err(format!(
                "job {id} is {}; the agent already exited",
                job.state
            ));
        }
        let target = match job.agent.as_str() {
            "runtime" | "orkia" => None,
            // Attach is interactive — guessing a stage of a multi-agent
            // pipeline would splice the wrong session. Fail closed.
            agent if agent.contains('|') => {
                return Err(format!(
                    "job {id} is a pipeline; attach a stage explicitly (available: {agent})"
                ));
            }
            agent => Some(agent.trim_start_matches('@').to_string()),
        };
        super::client_api::attach(&self.config, id, target)
    }
}

/// Build the bridge iff this process is the MAIN REPL (i.e. NOT a detached
/// runtime). `None` when `ORKIA_DETACHED_JOB_ID` is set so a runtime never folds
/// its own daemon's roster into its builtins.
pub(crate) fn provider(config: &ShellConfig) -> Option<Arc<dyn DaemonJobs>> {
    if std::env::var("ORKIA_DETACHED_JOB_ID").is_ok() {
        return None;
    }
    Some(Arc::new(DaemonJobsBridge {
        config: config.clone(),
    }))
}

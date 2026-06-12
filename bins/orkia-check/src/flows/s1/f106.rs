// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F106 — TUI cockpit daemon contract.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Instant;

use crate::report::FlowReport;
use orkia_e2e_harness::{AssertKind, HarnessError, OrkiaBinary, OrkiaSession};

use super::super::shared::*;
use super::F101_RELATED;

struct DaemonCleanup {
    bin: PathBuf,
    home: PathBuf,
}

impl Drop for DaemonCleanup {
    fn drop(&mut self) {
        let _ = Command::new(&self.bin)
            .env("HOME", &self.home)
            .current_dir(&self.home)
            .arg("pty-daemon-stop")
            .output();
    }
}

/// F106 — verify the public daemon CLI surfaces consumed by the cockpit TUI.
///
/// This flow intentionally avoids asserting ratatui pixels. The stable E2E
/// contract is that the TUI can drive `ps --json`, `daemon status`, `inspect`,
/// `logs`, `tell`, `stop`, and `wait` against a detached daemon job.
pub(crate) async fn flow_f106(session: &mut OrkiaSession) -> FlowReport {
    let id = "F106-tui-cockpit-daemon-contract";
    let name = "TUI cockpit daemon contract exposes ps/status/inspect/logs/tell/stop/wait";
    let t0 = Instant::now();
    let mut stages = Vec::<String>::new();
    let related: Vec<String> = F101_RELATED.iter().map(|s| s.to_string()).collect();

    if let Err(e) = boot_login(session).await {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "boot_login",
            &e,
            "See F101 boot_login hypothesis.",
            &related,
            session,
        );
    }
    stages.push("boot_login".into());

    if let Err(e) = session.seed_agent_with_script(
        "faye",
        &orkia_e2e_harness::scripts::keepalive_script("faye"),
    ) {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "seed_keepalive_agent",
            &e,
            "If seed failed: check `seed_agent_with_script` and fake-agent availability.",
            &related,
            session,
        );
    }
    stages.push("seed_keepalive_agent".into());

    let Some(shell) = session.shell() else {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "resolve_shell_home",
            &HarnessError::Infra("shell not booted".into()),
            "The daemon cockpit contract flow needs the harness shell sandbox.",
            &related,
            session,
        );
    };
    let home = shell.sandbox.home().to_path_buf();
    let bin = match OrkiaBinary::resolve(false) {
        Ok(bin) => bin.path().to_path_buf(),
        Err(e) => {
            return fail_with_diagnostics(
                id,
                name,
                t0,
                &stages,
                "resolve_orkia_bin",
                &HarnessError::Infra(format!("resolve orkia binary: {e}")),
                "Set ORKIA_TEST_BIN or build the `orkia` binary before running orkia-check.",
                &related,
                session,
            );
        }
    };
    let _cleanup = DaemonCleanup {
        bin: bin.clone(),
        home: home.clone(),
    };

    if let Err(e) = run_orkia(&bin, &home, &["--detach", "-c", "@faye cockpit-e2e"]) {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "spawn_detached_job",
            &e,
            "If detach fails: check pty_daemon spawn path and that faye is pre-seeded in the sandbox.",
            &related,
            session,
        );
    }
    stages.push("spawn_detached_job".into());

    let ps = match run_orkia(&bin, &home, &["ps", "--json"]) {
        Ok(output) => output,
        Err(e) => {
            return fail_with_diagnostics(
                id,
                name,
                t0,
                &stages,
                "ps_json",
                &e,
                "The cockpit loads its rows from `orkia ps --json`; this command must stay stable.",
                &related,
                session,
            );
        }
    };
    let ps_json: serde_json::Value = match serde_json::from_slice(&ps.stdout) {
        Ok(json) => json,
        Err(e) => {
            return fail_with_diagnostics(
                id,
                name,
                t0,
                &stages,
                "ps_json_parse",
                &HarnessError::Json(e),
                "The cockpit parser requires valid JSON from `ps --json`.",
                &related,
                session,
            );
        }
    };
    let Some(job) = ps_json
        .get("jobs")
        .and_then(serde_json::Value::as_array)
        .and_then(|jobs| jobs.first())
    else {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "ps_json_job",
            &json_assertion("expected at least one daemon job", &ps_json),
            "`ps --json` must expose jobs for the cockpit table.",
            &related,
            session,
        );
    };
    let Some(stage_id) = job
        .get("stages")
        .and_then(serde_json::Value::as_array)
        .and_then(|stages| stages.first())
        .and_then(|stage| stage.get("id"))
        .and_then(serde_json::Value::as_u64)
    else {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "ps_json_stage",
            &json_assertion(
                "expected first daemon job to include a numeric stage id",
                &ps_json,
            ),
            "Stage selections in the TUI use the public `job_id:stage_id` target form.",
            &related,
            session,
        );
    };
    if job.get("attachable").and_then(serde_json::Value::as_bool) != Some(true) {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "ps_json_attachable",
            &json_assertion("expected detached keepalive job to be attachable", &ps_json),
            "The cockpit disables attach for non-attachable rows; live jobs must report attachable=true.",
            &related,
            session,
        );
    }
    stages.push("ps_json".into());

    if let Err(e) = run_orkia(&bin, &home, &["daemon", "status"])
        .and_then(|out| stdout_contains(out, "state: running"))
        .and_then(|out| stdout_contains(out, "jobs: 1"))
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "daemon_status",
            &e,
            "The cockpit top bar depends on daemon status state/pid/job count.",
            &related,
            session,
        );
    }
    stages.push("daemon_status".into());

    if let Err(e) = run_orkia(&bin, &home, &["inspect", "1"])
        .and_then(|out| stdout_contains(out, "JOB 1"))
        .and_then(|out| stdout_contains(out, "attachable: true"))
        .and_then(|out| stdout_contains(out, "stages:"))
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "inspect_panel_contract",
            &e,
            "The TUI inspect panel shells out to `orkia inspect <id>` and renders this text.",
            &related,
            session,
        );
    }
    stages.push("inspect_panel_contract".into());

    if let Err(e) = run_orkia(&bin, &home, &["logs", "1", "--last", "20"])
        .and_then(|out| stdout_contains(out, "detached.spawn"))
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "logs_panel_contract",
            &e,
            "The TUI logs panel shells out to `orkia logs <id> --last N`.",
            &related,
            session,
        );
    }
    stages.push("logs_panel_contract".into());

    let target = format!("1:{stage_id}");
    if let Err(e) = run_orkia(&bin, &home, &["tell", &target, "hello-from-tui-e2e"]) {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "tell_stage_target",
            &e,
            "Stage-row tell must use the public `job_id:stage_id` target form.",
            &related,
            session,
        );
    }
    stages.push("tell_stage_target".into());

    if let Err(e) = run_orkia(&bin, &home, &["stop", "1"]) {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "stop_job",
            &e,
            "The TUI `s` action uses `orkia stop <job_id>` for graceful job-level termination.",
            &related,
            session,
        );
    }
    stages.push("stop_job".into());

    if let Err(e) = run_orkia(&bin, &home, &["wait", "1", "--timeout", "3"])
        .and_then(|out| stdout_contains(out, "1 stopped"))
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "wait_stopped",
            &e,
            "After a cockpit stop action, `wait` must observe the stopped terminal state.",
            &related,
            session,
        );
    }
    stages.push("wait_stopped".into());

    if let Err(e) = run_orkia(&bin, &home, &["inspect", "1"])
        .and_then(|out| stdout_contains(out, "status: stopped"))
        .and_then(|out| stdout_contains(out, "attachable: false"))
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "inspect_stopped_contract",
            &e,
            "Stopped jobs must remain inspectable while becoming non-attachable in the cockpit.",
            &related,
            session,
        );
    }
    stages.push("inspect_stopped_contract".into());

    pass_report(id, name, t0, stages)
}

fn run_orkia(bin: &Path, home: &Path, args: &[&str]) -> Result<Output, HarnessError> {
    let output = Command::new(bin)
        .env("HOME", home)
        .current_dir(home)
        .args(args)
        .output()
        .map_err(HarnessError::Io)?;
    if output.status.success() {
        return Ok(output);
    }
    Err(HarnessError::assertion(
        format!("orkia {:?} failed with status {}", args, output.status),
        AssertKind::Output,
        command_output_state(&output),
    ))
}

fn stdout_contains(output: Output, needle: &str) -> Result<Output, HarnessError> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.contains(needle) {
        return Ok(output);
    }
    Err(HarnessError::assertion(
        format!("stdout missing `{needle}`"),
        AssertKind::Output,
        command_output_state(&output),
    ))
}

fn command_output_state(output: &Output) -> String {
    format!(
        "--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn json_assertion(message: &str, json: &serde_json::Value) -> HarnessError {
    HarnessError::assertion(message, AssertKind::Output, json.to_string())
}

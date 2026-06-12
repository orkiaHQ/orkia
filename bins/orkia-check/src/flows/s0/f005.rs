// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F005 — agent job control: spawn, ps, attach, detach, kill, ps empty.

use super::super::shared::*;
use crate::report::FlowReport;
use orkia_e2e_harness::OrkiaSession;
use std::time::{Duration, Instant};

/// F005 — agent job control: spawn → ps → attach → detach → kill → ps empty.
/// Exercises the load-bearing PTY paths: agent dispatch, raw-byte attach,
/// Ctrl-Z detach, signal-based kill, and lifecycle journal envelopes.
pub(crate) async fn flow_f005(session: &mut OrkiaSession) -> FlowReport {
    let id = "F005-agent-job-control";
    let name = "Spawn / ps / attach / detach / kill an agent";
    let t0 = Instant::now();
    let mut stages = Vec::<String>::new();
    let related = vec!["shell".to_string()];

    let Some(shell) = session.shell() else {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "boot",
            "INFRA_UNREACHABLE",
            "orkia shell not booted".into(),
            "Set ORKIA_TEST_BIN and ORKIA_TEST_FAKE_AGENT_BIN.",
            &related,
        );
    };

    // Long-lived faye script: print a ready marker, then sleep so the
    // agent stays alive for attach/detach/kill.
    let script_path = shell
        .data_dir
        .join("agents")
        .join("faye")
        .join("script.yaml");
    let script = "name: faye-f005\nraw_mode: false\nsteps:\n  - kind: print\n    text: \"faye-ready>\\n\"\n  - kind: sleep\n    ms: 60000\n  - kind: exit\n    code: 0\n";
    if let Err(e) = std::fs::write(&script_path, script) {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "boot",
            "RUNTIME_ERROR",
            format!("rewrite faye script: {e}"),
            "Sandbox layout changed; check seed_faye_agent path.",
            &related,
        );
    }

    if let Err(e) = session
        .wait_for("\x1b]133;A", Duration::from_secs(10))
        .await
    {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "boot",
            "TIMEOUT",
            format!("{e}"),
            "Initial prompt never reached OSC 133;A.",
            &related,
        );
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    stages.push("boot".into());

    // Session pre-loaded by the harness's real boot-time login; no
    // in-shell `login` (interactive magic-link can't complete headless).

    // Initial ps — no agents yet.
    if let Err(e) = session
        .run("ps", "none running", Duration::from_secs(5))
        .await
    {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "ps_empty_initial",
            &classify(&e),
            format!("{e}"),
            "If 'none running' marker absent: check `push_agents_section` in orkia-builtin/src/ps.rs (empty-state branch). \
             If a prior flow leaked an agent: confirm `reset_for_next_flow` ran and faye actually exited.",
            &related,
        );
    }
    stages.push("ps_empty_initial".into());

    // Spawn faye in the background. `@faye` without a pipe routes
    // through dispatch_agent; for non-hook providers the body is queued
    // and stdin is PTY. The shell prints "[N] spawned" on success.
    if let Err(e) = session
        .run("@faye", "spawned", Duration::from_secs(10))
        .await
    {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "spawn_faye",
            &classify(&e),
            format!("{e}"),
            "If 'spawned' marker absent: check `dispatch_agent` in orkia-shell/src/repl.rs and `config.resolve_agent(\"faye\")`. \
             agent.toml must have `[runtime] command = <fake-agent path>` — NOT `command` under `[agent]` (silent fallback to `claude` from $PATH).",
            &related,
        );
    }
    stages.push("spawn_faye".into());

    // ps should now list faye. Avoid the "none running" wait pattern by
    // checking for "faye" in the output via wait_for.
    if let Err(e) = session.run("ps", "faye", Duration::from_secs(5)).await {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "ps_shows_faye",
            &classify(&e),
            format!("{e}"),
            "If 'faye' missing: check the agent-row rendering in `push_agents_section` (orkia-builtin/src/ps.rs). \
             If faye died right after spawn: check the fake-agent script — for F005 it sleeps 60s so it stays alive for ps/attach/kill.",
            &related,
        );
    }
    stages.push("ps_shows_faye".into());

    // attach — enters raw-byte splice mode (`raw_attach::run_foreground`).
    // The agent's earlier "faye-ready>" print should be visible.
    if let Err(e) = session
        .run("attach @faye", "faye-ready>", Duration::from_secs(10))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "attach_faye",
            &e,
            "If the screen shows a *different* program (e.g. Claude Code's banner), check \
             `<sandbox>/.orkia/agents/faye/agent.toml` — `command` must be under `[runtime]`, not `[agent]`. \
             If raw mode entered but no agent output: check `orkia-shell::job::raw_attach::run_foreground` byte-splice. \
             If raw mode never entered: check `orkia-shell-tui` renderer's `drive_attached`.",
            &related,
            session,
        );
    }
    stages.push("attach_faye".into());

    // detach — send 0x1a (Ctrl-Z), one of the bytes raw_attach scans
    // for in DETACH_KEYS. The shell exits raw mode and redraws the
    // prompt; we wait for the unique scope-less prompt-ready marker.
    if let Err(e) = session.send_bytes(&[0x1a]) {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "detach_faye",
            "RUNTIME_ERROR",
            format!("write Ctrl-Z: {e}"),
            "PTY write failed; the master fd may be closed.",
            &related,
        );
    }
    if let Err(e) = session.wait_for("\x1b]133;A", Duration::from_secs(5)).await {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "detach_faye",
            &classify(&e),
            format!("{e}"),
            "If no new OSC 133;A after Ctrl-Z: check `raw_attach::DETACH_KEYS` in orkia-shell/src/job/raw_attach.rs (0x1a must be present). \
             If detach ran but prompt didn't redraw: check the `AttachedOutcome::Detached` handler in `run_foreground_attached`.",
            &related,
        );
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    stages.push("detach_faye".into());

    // ps after detach — faye is still alive (detach does not kill).
    if let Err(e) = session.run("ps", "faye", Duration::from_secs(5)).await {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "ps_after_detach",
            &classify(&e),
            format!("{e}"),
            "If faye missing from ps after detach: Ctrl-Z killed instead of detaching. Check `raw_attach::run_foreground` — \
             must return DETACH_CODE (`-100`) not a signal-exit code. The JobController must keep the entry in Running state.",
            &related,
        );
    }
    stages.push("ps_after_detach".into());

    // kill faye — handle_kill routes through resolve_kill which knows
    // agent jobs by name. Expected: shell prints "[N] stopped" and a
    // lifecycle:completed envelope hits journal.jsonl.
    if let Err(e) = session
        .run("kill faye", "stopped", Duration::from_secs(10))
        .await
    {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "kill_faye",
            &classify(&e),
            format!("{e}"),
            "If 'stopped' marker absent: check `handle_kill` in orkia-shell/src/repl.rs → `resolve_kill` (orkia-builtin) → `JobController::stop`. \
             The output format is `[<job-id>] stopped`.",
            &related,
        );
    }
    stages.push("kill_faye".into());

    // Journal must show a terminal lifecycle envelope for the killed
    // job. `kill` via `self.jobs.stop(id)` emits `JobEvent::Stopped`
    // (`event:"stopped"`); natural exit would emit `Completed`
    // (`event:"completed"`). We just check for ANY lifecycle event in
    // this flow's window — flow's cursor was advanced by
    // reset_for_next_flow, so we only see this flow's lifecycle.
    tokio::time::sleep(Duration::from_millis(300)).await;
    if let Err(e) = session.journal().has_envelope("lifecycle").await {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "assert_journal_terminal",
            &e,
            "If 0 lifecycle events since flow start: kill never reached `emit_lifecycle_envelope` in repl.rs. \
             Confirm `handle_kill` route via `self.jobs.stop(id)` and that the `JobEvent` channel is connected.",
            &related,
            session,
        );
    }
    stages.push("assert_journal_terminal".into());

    // Final ps — back to empty.
    if let Err(e) = session
        .run("ps", "none running", Duration::from_secs(5))
        .await
    {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "ps_empty_final",
            &classify(&e),
            format!("{e}"),
            "If faye still in ps after kill: `JobController::stop` returned but the entry wasn't removed. \
             Check the `JobEvent::Stopped` → `reap` path in orkia-shell/src/job/.",
            &related,
        );
    }
    stages.push("ps_empty_final".into());

    FlowReport {
        id: id.into(),
        name: name.into(),
        status: crate::report::FlowStatus::Pass,
        duration_ms: elapsed_ms(t0),
        env_group: String::new(),
        stages_completed: stages,
        stage_failed: None,
        failure: None,
    }
}

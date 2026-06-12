// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F004 — pipe shell stdout into an agent via `<cmd> | @faye <args>`.

use super::super::shared::*;
use crate::report::FlowReport;
use orkia_e2e_harness::OrkiaSession;
use std::time::{Duration, Instant};

/// F004 — pipe shell stdout into an agent via `<cmd> | @faye <args>`.
/// Rewrites the faye script in-place to expect a sentinel on stdin and
/// print a confirmation marker, then runs the pipe and asserts both
/// the marker appears AND the `shell.pipe.input` journal envelope fired.
pub(crate) async fn flow_f004(session: &mut OrkiaSession) -> FlowReport {
    let id = "F004-shell-pipe-to-agent";
    let name = "Pipe shell stdout into an agent via the | @faye syntax";
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

    // Rewrite the faye script for F004: wait for the sentinel on stdin,
    // print a confirmation, exit. Must happen before we type the pipe.
    let script_path = shell
        .data_dir
        .join("agents")
        .join("faye")
        .join("script.yaml");
    let script = "name: faye-f004\nraw_mode: false\nsteps:\n  - kind: await_input\n    until: \"F004-DONE\"\n    timeout_ms: 5000\n  - kind: print\n    text: \"faye-received\\n\"\n  - kind: exit\n    code: 0\n";
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

    // Stage `run_pipe` — single echo carries the sentinel. We wait for
    // the shell's "X bytes captured" trace, which proves the pipe
    // parser ran, the shell-stage captured stdout, and the agent was
    // dispatched. We do NOT wait for the fake-agent to print confirmation
    // of receiving stdin because `StdinSource::InitialBytes → PTY master
    // write` is racy (documented at job/spawn.rs:179). The journal
    // envelope check below is the load-bearing assertion.
    if let Err(e) = session
        .run(
            "echo F004-DONE | @faye summarize",
            "bytes captured",
            Duration::from_secs(10),
        )
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "run_pipe",
            &e,
            "If `parse_shell_to_agent` not invoked: check `orkia-shell::shell_agent_pipe::find_shell_agent_split`. \
             If shell stage ran but spawn didn't: check `dispatch_shell_to_agent` agent-resolution path. \
             If agent spawned but its output never reached the screen: check raw-attach byte splice.",
            &related,
            session,
        );
    }
    stages.push("run_pipe".into());

    // Give the journal writer a beat to flush before reading the tail.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Assert: a lifecycle `spawn` envelope hit `journal.jsonl` since
    // the flow started (reset_for_next_flow advanced the cursor so we
    // ignore prior flows' spawns). The custom `shell.pipe.input` event
    // also fires but flows through the SEAL/event-router sink — there
    // is no `Custom` variant in `EventType`, so it never reaches the
    // journal file.
    if let Err(e) = session.journal().has_envelope("lifecycle").await {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "assert_journal_spawn",
            &e,
            "If 0 lifecycle events: pipe never reached spawn — check `orkia-shell/job/spawn.rs::spawn_agent`. \
             If the wrong agent spawned: check `<sandbox>/.orkia/agents/<name>/agent.toml [runtime] command`.",
            &related,
            session,
        );
    }
    stages.push("assert_journal_spawn".into());

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

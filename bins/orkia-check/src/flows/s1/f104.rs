// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F104 — natural completion.

use std::time::{Duration, Instant};

use crate::report::FlowReport;
use orkia_e2e_harness::OrkiaSession;

use super::super::shared::*;
use super::F101_RELATED;

/// F104 — natural exit produces lifecycle:completed with exit_code=0.
pub(crate) async fn flow_f104(session: &mut OrkiaSession) -> FlowReport {
    let id = "F104-natural-completion";
    let name = "Agent that exits naturally produces lifecycle:completed exit_code=0";
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
        &orkia_e2e_harness::scripts::natural_exit_script("faye"),
    ) {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "seed",
            &e,
            "Seed failed.",
            &related,
            session,
        );
    }
    stages.push("seed".into());

    // Spawn marker is the shell's "[N] spawned" line; agent's own
    // "starting work" print is not visible (background, no attach).
    if let Err(e) = session
        .run("@faye start", "spawned: agent:faye", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "spawn",
            &e,
            "Same as F101.",
            &related,
            session,
        );
    }
    // Wait for the agent to exit naturally, then force orkia to reap
    // it (see `OrkiaSession::force_reap` doc for the architectural
    // reason). Without force_reap nothing polls `jobs.list()` between
    // prompts, so `lifecycle:completed` never gets emitted.
    tokio::time::sleep(Duration::from_secs(3)).await; // script sleeps 2s + Exit
    if let Err(e) = session.force_reap().await {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "spawn",
            &e,
            "If force_reap timed out: `jobs` builtin didn't complete. Check `handle_jobs` in repl.rs.",
            &related,
            session,
        );
    }
    stages.push("spawn".into());

    // Wait for lifecycle:completed with exit_code=0. The script sleeps
    // 2s + exits 0. Budget: 5s.
    let exit_zero_pred = |e: &orkia_e2e_harness::JournalEvent| {
        e.event_type() == Some("lifecycle")
            && e.event() == Some("completed")
            && e.get("exit_code").and_then(|v| v.as_i64()) == Some(0)
    };
    if let Err(e) = session
        .journal()
        .wait_for_envelope_with(
            exit_zero_pred,
            Duration::from_secs(5),
            "lifecycle:completed exit_code=0",
        )
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "wait_for_completion",
            &e,
            "If no Completed event: check that the fake-agent actually exited (look for the 'done' print). \
             If a Completed appeared but exit_code != 0: the script failed before reaching `Exit{code:0}`. \
             If a Stopped event appeared instead: the JobController's reap path is misrouting exits — \
             check `JobController::reap` in orkia-shell/src/job/mod.rs:557.",
            &related,
            session,
        );
    }
    stages.push("wait_for_completion".into());

    if let Err(e) = session
        .run("ps", "none running", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "ps_empty",
            &e,
            "If faye still in ps after natural exit: terminal-state filter is missing. \
             Check `reap` retain: `!matches!(j.state, Done {..} | Failed {..})`.",
            &related,
            session,
        );
    }
    stages.push("ps_empty".into());

    pass_report(id, name, t0, stages)
}

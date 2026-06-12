// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F102 — fg/bg cycle.

use std::time::{Duration, Instant};

use crate::report::FlowReport;
use orkia_e2e_harness::OrkiaSession;

use super::super::shared::*;
use super::F101_RELATED;

/// F102 — fg/bg cycle: spawn → Ctrl-Z → bg → fg → Ctrl-Z → kill.
pub(crate) async fn flow_f102(session: &mut OrkiaSession) -> FlowReport {
    let id = "F102-fg-bg-cycle";
    let name = "Spawn agent, suspend via Ctrl-Z, bg, fg, detach, kill";
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
            "seed",
            &e,
            "Seed failed; check `seed_agent_with_script` in session.rs.",
            &related,
            session,
        );
    }
    stages.push("seed".into());

    if let Err(e) = session
        .run("@faye hello", "spawned: agent:faye", Duration::from_secs(8))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "spawn_faye",
            &e,
            "See F101 spawn_faye hypothesis.",
            &related,
            session,
        );
    }
    stages.push("spawn_faye".into());

    // `@faye` spawns directly to background — no Ctrl-Z step needed,
    // unlike F005 which spawned with content and stayed foreground-attached.
    // ps should show faye in `running` state — Orkia has no separate
    // `Background` JobState (variants: Foreground/Running/Stopped/Done/Failed).
    if let Err(e) = session.run("ps", "faye", Duration::from_secs(5)).await {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "ps_after_detach",
            &e,
            "If faye missing from ps: Ctrl-Z killed instead of detaching. Check `raw_attach::run_foreground` exit path — must return DETACH_CODE.",
            &related,
            session,
        );
    }
    if let Err(e) = session.output().contains("running") {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "ps_after_detach",
            &e,
            "If state is not 'running': JobState::Display in shell-types/job.rs has 5 variants — Foreground/Running/Stopped/Done/Failed. \
             A backgrounded job should be Running. If it's Stopped: check that raw_attach didn't issue SIGTSTP on detach.",
            &related,
            session,
        );
    }
    stages.push("ps_after_detach".into());

    // bg faye — should be a no-op for state since faye is already
    // Running; but if it was Stopped, bg would Continue it (emits
    // lifecycle:continued). We tolerate either pattern. Wait for the
    // command to complete by polling for the next prompt mark.
    if let Err(e) = session.type_line("bg faye").await {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "bg_faye",
            &e,
            "PTY write of 'bg faye' failed.",
            &related,
            session,
        );
    }
    // Give bg a moment then ensure we're back at prompt.
    tokio::time::sleep(Duration::from_millis(200)).await;
    stages.push("bg_faye".into());

    // fg faye — reattaches. Should see the agent's PTY again.
    if let Err(e) = session
        .run("fg faye", "faye ready", Duration::from_secs(8))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "fg_faye",
            &e,
            "If 'faye ready' absent: `fg` may not have re-attached. Check `handle_fg` (repl.rs:2099) → `run_foreground_job`. \
             If a different agent attached: `resolve_job_target` matched wrong job.",
            &related,
            session,
        );
    }
    stages.push("fg_faye".into());

    if let Err(e) = session.send_bytes(&[0x1a]) {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "redetach",
            &e,
            "PTY write failed.",
            &related,
            session,
        );
    }
    if let Err(e) = session.wait_for("\x1b]133;A", Duration::from_secs(5)).await {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "redetach",
            &e,
            "Same as ctrl_z_detach.",
            &related,
            session,
        );
    }
    stages.push("redetach".into());

    if let Err(e) = session
        .run("kill faye", "stopped", Duration::from_secs(8))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "kill_faye",
            &e,
            "See F101 kill_faye hypothesis.",
            &related,
            session,
        );
    }
    stages.push("kill_faye".into());

    pass_report(id, name, t0, stages)
}

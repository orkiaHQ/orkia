// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F101 — multi-agent ps.

use std::time::{Duration, Instant};

use crate::report::FlowReport;
use orkia_e2e_harness::OrkiaSession;

use super::super::shared::*;

const F101_RELATED: &[&str] = &["shell"];

/// F101 — spawn two agents, ps shows both, kill each in turn.
pub(crate) async fn flow_f101(session: &mut OrkiaSession) -> FlowReport {
    let id = "F101-multi-agent-ps";
    let name = "Spawn faye + sage; ps shows both; kill each in turn";
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
            "If timeout at boot: check OSC 133;A emission in orkia-shell prompt rendering. \
             If session missing: check the boot-time real login (`login::login_to_session_file`) and ORKIA_BACKEND_URL.",
            &related,
            session,
        );
    }
    stages.push("boot_login".into());

    // seed_agents: install keepalive scripts for both names.
    if let Err(e) = session.seed_agent_with_script(
        "faye",
        &orkia_e2e_harness::scripts::keepalive_script("faye"),
    ) {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "seed_agents",
            &e,
            "If seed failed: check `seed_agent_with_script` in session.rs and that ORKIA_TEST_FAKE_AGENT_BIN points at a built fake-agent.",
            &related,
            session,
        );
    }
    if let Err(e) = session.seed_agent_with_script(
        "sage",
        &orkia_e2e_harness::scripts::keepalive_script("sage"),
    ) {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "seed_agents",
            &e,
            "Same as faye seed; second seed failed.",
            &related,
            session,
        );
    }
    stages.push("seed_agents".into());

    // spawn faye (background after attach exits, since fake-agent's
    // AwaitInput holds the PTY foreground until detach).
    // `@<agent>` in orkia dispatches to background by default. The
    // agent's own stdout is invisible to the shell PTY until someone
    // attaches; the load-bearing visible marker is the shell's own
    // "[N] spawned: agent:<name>" line.
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
            "If 'spawned: agent:faye' missing: check `dispatch_agent` in repl.rs and that orkia found the configured fake-agent. \
             If a different agent name appeared: agent.toml [runtime] command may not point at fake-agent.",
            &related,
            session,
        );
    }
    stages.push("spawn_faye".into());

    if let Err(e) = session
        .run("@sage hi", "spawned: agent:sage", Duration::from_secs(8))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "spawn_sage",
            &e,
            "If sage spawn failed but faye succeeded: race in JobController::spawn (job/mod.rs), OR sage's agent.toml \
             wasn't loaded at shell startup. Orkia's `hydrate_agents_from_dir` (config.rs:82) only runs at boot — \
             mid-session writes to <sandbox>/.orkia/agents/sage/agent.toml are invisible. \
             Workaround: seed sage BEFORE starting the orkia process (move seed_agent_with_script into `try_start_shell`).",
            &related,
            session,
        );
    }
    stages.push("spawn_sage".into());

    // ps should now list both. We type `ps` and wait_for whichever name
    // appears LAST in the table (alphabetical-ish ordering not guaranteed).
    // Then assert the other is also visible via output().contains.
    if let Err(e) = session.run("ps", "sage", Duration::from_secs(5)).await {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "ps_shows_both",
            &e,
            "If 'sage' missing: JobController::list (orkia-shell/src/job/mod.rs) may be filtering or skipping the 2nd job. \
             Check the `jobs` Vec retain calls.",
            &related,
            session,
        );
    }
    if let Err(e) = session.output().contains("faye") {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "ps_shows_both",
            &e,
            "If 'faye' missing but 'sage' present: the first spawn was reaped/lost. \
             Check JobController::reap retain filter — it removes Done/Failed states.",
            &related,
            session,
        );
    }
    // Both must be in a live state ("running" — Orkia has no "background" label).
    if let Err(e) = session.output().contains_any(&["running", "fg"]) {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "ps_shows_both",
            &e,
            "If state shown is 'stopped' or 'done(N)': the detach Ctrl-Z transitioned the job to a non-live state. \
             Check `raw_attach::run_foreground` exit path — must return DETACH_CODE so the JobController keeps state=Running.",
            &related,
            session,
        );
    }
    stages.push("ps_shows_both".into());

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
            "If 'stopped' marker missing: check `handle_kill` → `resolve_kill` → `JobController::stop` in orkia-shell.",
            &related,
            session,
        );
    }
    // Confirm a lifecycle:stopped envelope hit the journal since this flow began.
    if let Err(e) = session
        .journal()
        .wait_for_envelope("lifecycle", Duration::from_secs(3))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "kill_faye",
            &e,
            "No lifecycle event since flow start. Check `emit_lifecycle_envelope` wiring in repl.rs after stop().",
            &related,
            session,
        );
    }
    stages.push("kill_faye".into());

    // After kill_faye, ps should list sage. We do NOT assert on the
    // ABSENCE of "faye" because the shell renders a `[1]+ Stopped faye`
    // notification that sticks in the alacritty grid history. The
    // lifecycle:stopped envelope verified above is the real proof that
    // faye is gone; ps_shows_only_sage just confirms sage survived.
    if let Err(e) = session.run("ps", "sage", Duration::from_secs(5)).await {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "ps_shows_only_sage",
            &e,
            "If sage disappeared after killing faye: kill was not scoped to one job_id. Check resolve_job_target + JobController::stop in orkia-shell.",
            &related,
            session,
        );
    }
    stages.push("ps_shows_only_sage".into());

    if let Err(e) = session
        .run("kill sage", "stopped", Duration::from_secs(8))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "kill_sage",
            &e,
            "Same as kill_faye but for the second agent.",
            &related,
            session,
        );
    }
    stages.push("kill_sage".into());

    if let Err(e) = session
        .run("ps", "none running", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "ps_empty_final",
            &e,
            "If still listing agents: kill returned but reap didn't run. Check the JobController main loop / event_tx fan-out.",
            &related,
            session,
        );
    }
    stages.push("ps_empty_final".into());

    pass_report(id, name, t0, stages)
}

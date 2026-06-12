// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F105 — crash recovery.

use std::time::{Duration, Instant};

use crate::report::FlowReport;
use orkia_e2e_harness::OrkiaSession;

use super::super::shared::*;
use super::F101_RELATED;

/// F105 — agent abort detected via lifecycle:completed with exit_code != 0;
/// next agent spawn after the crash works (no state corruption).
pub(crate) async fn flow_f105(session: &mut OrkiaSession) -> FlowReport {
    let id = "F105-crash-recovery";
    let name = "Agent that aborts (SIGABRT) is detected via non-zero exit_code; next spawn works";
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

    if let Err(e) =
        session.seed_agent_with_script("faye", &orkia_e2e_harness::scripts::crash_script("faye"))
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "seed_crash",
            &e,
            "Seed failed.",
            &related,
            session,
        );
    }
    stages.push("seed_crash".into());

    // Spawn faye — it will print "about to crash" (invisible: background)
    // then call std::process::abort() → SIGABRT (signal 6, exit code 134).
    if let Err(e) = session
        .run("@faye go", "spawned: agent:faye", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "spawn_crash",
            &e,
            "If marker missing: dispatch_agent didn't reach spawn. See F101 hypothesis.",
            &related,
            session,
        );
    }
    // Agent should die almost immediately (Print → Crash). Give it
    // 500ms then force_reap (see OrkiaSession::force_reap doc — orkia's
    // SIGCHLD handling between prompts doesn't trigger reap on its own).
    tokio::time::sleep(Duration::from_millis(500)).await;
    if let Err(e) = session.force_reap().await {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "spawn_crash",
            &e,
            "If force_reap timed out: `jobs` builtin didn't complete. Check `handle_jobs` in repl.rs.",
            &related,
            session,
        );
    }
    stages.push("spawn_crash".into());

    // After the crash, the foreground attach should exit. We poll for
    // either a new prompt mark OR a lifecycle:completed envelope with
    // non-zero exit_code, whichever arrives first.
    let crash_pred = |e: &orkia_e2e_harness::JournalEvent| {
        e.event_type() == Some("lifecycle")
            && e.event() == Some("completed")
            && e.get("exit_code")
                .and_then(|v| v.as_i64())
                .is_some_and(|c| c != 0)
    };
    if let Err(e) = session
        .journal()
        .wait_for_envelope_with(
            crash_pred,
            Duration::from_secs(5),
            "lifecycle:completed with exit_code != 0",
        )
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "wait_for_crash_envelope",
            &e,
            "If no non-zero Completed event: orkia may be coercing signal-exits to 0. \
             Check `JobEntry::try_exit_code` (orkia-shell/src/job/entry.rs:37) — it must propagate the signal-encoded form. \
             portable-pty's `try_wait` returns `Option<ExitStatus>`; verify the i32 conversion preserves signal info.",
            &related,
            session,
        );
    }
    stages.push("wait_for_crash_envelope".into());

    // Drain back to a shell prompt (the attach exited; orkia should have
    // redrawn the prompt by now, but give it a beat).
    let _ = session.wait_for("\x1b]133;A", Duration::from_secs(3)).await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Now the load-bearing assertion: after a crashed agent, a fresh
    // spawn of a DIFFERENT agent must work. Tests state-corruption /
    // mutex-poisoning / PTY-leak.
    if let Err(e) = session.seed_agent_with_script(
        "sage",
        &orkia_e2e_harness::scripts::keepalive_script("sage"),
    ) {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "spawn_next_agent_after_crash",
            &e,
            "Seed failed; may indicate the sandbox is partially corrupted.",
            &related,
            session,
        );
    }
    if let Err(e) = session
        .run("@sage hello", "spawned: agent:sage", Duration::from_secs(8))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "spawn_next_agent_after_crash",
            &e,
            "If sage doesn't spawn after faye crash: state corruption. \
             Possible causes: mutex poisoning in JobController, dangling PTY handle, \
             agent_commands map not refreshed. Check the reap path in job/mod.rs:557 for partial cleanup.",
            &related,
            session,
        );
    }
    stages.push("spawn_next_agent_after_crash".into());

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
            "See F101 kill hypothesis.",
            &related,
            session,
        );
    }
    stages.push("kill_sage".into());

    pass_report(id, name, t0, stages)
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F103 — wait and disown.

use std::time::{Duration, Instant};

use crate::report::FlowReport;
use orkia_e2e_harness::{AssertKind, HarnessError, OrkiaSession};

use super::super::shared::*;
use super::{F101_RELATED, count_lifecycle_events, recent_lifecycle_events_after};

/// F103 — wait blocks until job done; disown detaches without killing.
pub(crate) async fn flow_f103(session: &mut OrkiaSession) -> FlowReport {
    let id = "F103-wait-and-disown";
    let name = "wait blocks until job done; disown detaches without killing";
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

    // === Part A — wait blocks ===
    if let Err(e) = session.seed_agent_with_script(
        "faye",
        &orkia_e2e_harness::scripts::long_work_script("faye"),
    ) {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "seed_longwork",
            &e,
            "Seed failed.",
            &related,
            session,
        );
    }
    // Spawn goes to background; agent's "long work starting" print is
    // not visible until attached. We just need the shell's spawn marker.
    if let Err(e) = session
        .run("@faye start", "spawned: agent:faye", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "spawn_longwork",
            &e,
            "Same as F101 spawn_faye.",
            &related,
            session,
        );
    }
    stages.push("spawn_longwork".into());

    // wait_blocks_5s: type `wait faye`, measure how long until next prompt
    // appears. handle_wait produces no output; we detect completion by
    // counting OSC 133;D (command-end) marks.
    //
    // Pre-check: ensure faye is actually alive in the job table. A
    // short settle delay also avoids racing the spawn dispatch.
    tokio::time::sleep(Duration::from_millis(200)).await;
    if let Err(e) = session.run("ps", "faye", Duration::from_secs(5)).await {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "wait_blocks_5s_precheck",
            &e,
            "If 'faye' not in ps right after spawn: agent died before we could wait on it. \
             Check that long_work_script's Sleep step didn't get truncated; \
             or that the script.yaml write happened before @faye spawned (no race).",
            &related,
            session,
        );
    }
    // Let the precheck's 133;D fully land before snapshotting pre_count.
    // `session.run` returns as soon as the body marker ("faye") appears,
    // BEFORE the command emits its OSC 133;D. Without this settle the
    // wait loop sees the precheck's command-end as a false positive.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let pre_count = session.command_end_count();
    let wait_start = Instant::now();
    if let Err(e) = session.type_line("wait faye").await {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "wait_blocks_5s",
            &e,
            "type_line failed.",
            &related,
            session,
        );
    }
    // Poll until a new 133;D (command-end) mark appears OR 8s elapse
    // (5s script + 3s slack). 133;D only fires when wait actually
    // returns — 133;A fires on every keystroke and would false-trigger.
    loop {
        if session.command_end_count() > pre_count {
            break;
        }
        if wait_start.elapsed() > Duration::from_secs(8) {
            return fail_with_diagnostics(
                id,
                name,
                t0,
                &stages,
                "wait_blocks_5s",
                &HarnessError::Timeout("wait never returned".into()),
                "wait should block until the agent leaves the job table. \
                 Check `handle_wait` poll loop in orkia-shell/src/repl.rs:1771 and \
                 `JobController::list` (which wait polls).",
                &related,
                session,
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let elapsed = wait_start.elapsed();
    if elapsed < Duration::from_secs(4) {
        let raw_tail = session
            .shell()
            .map(|s| {
                let raw = s.process.pty.raw_text();
                let start = raw.len().saturating_sub(2500);
                // Byte offset may land mid-codepoint and panic — walk back
                // to a char boundary (BUG-087).
                let start = (0..=start)
                    .rev()
                    .find(|&i| raw.is_char_boundary(i))
                    .unwrap_or(0);
                raw[start..].replace('\x1b', "\\e")
            })
            .unwrap_or_default();
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "wait_blocks_5s",
            &HarnessError::assertion(
                format!("wait returned too fast: {:?}", elapsed),
                AssertKind::Output,
                format!(
                    "expected ≥ 4.0s (script sleeps 5s), got {:.2}s\n--- raw tail ---\n{}",
                    elapsed.as_secs_f64(),
                    raw_tail
                ),
            ),
            "If shell shows 'wait: no job matching faye': the agent exited BEFORE we typed wait. \
             Check that long_work_script (scripts.rs) has Sleep:5000 and was actually written to \
             `<sandbox>/.orkia/agents/faye/script.yaml`. \
             If shell shows the prompt redrawing but no wait output: handle_wait returned without polling — \
             check the loop in repl.rs:1771.",
            &related,
            session,
        );
    }
    if elapsed > Duration::from_secs(8) {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "wait_blocks_5s",
            &HarnessError::Timeout(format!(
                "wait took {:.2}s, > 8s budget",
                elapsed.as_secs_f64()
            )),
            "wait took longer than 8s; either the script doesn't actually exit, \
             or wait is polling too slowly. Check `tokio::time::sleep(50ms)` cadence in handle_wait.",
            &related,
            session,
        );
    }
    stages.push("wait_blocks_5s".into());

    // === Part B — disown ===
    if let Err(e) = session.seed_agent_with_script(
        "sage",
        &orkia_e2e_harness::scripts::keepalive_script("sage"),
    ) {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "seed_disown",
            &e,
            "Seed failed.",
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
            "spawn_sage_disown",
            &e,
            "See F101 spawn hypothesis.",
            &related,
            session,
        );
    }
    stages.push("spawn_sage_disown".into());

    // Snapshot the journal-event count BEFORE disown so we can later
    // assert no new lifecycle:stopped appeared.
    let lifecycle_count_before = count_lifecycle_events(session).await;

    if let Err(e) = session
        .run("disown sage", "disowned", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "disown_sage",
            &e,
            "If 'disowned' missing: check `handle_disown` (repl.rs:1693). \
             Output format is '[<job-id>] disowned'.",
            &related,
            session,
        );
    }
    stages.push("disown_sage".into());

    if let Err(e) = session
        .run("ps", "none running", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "ps_after_disown",
            &e,
            "If sage still in ps after disown: `handle_disown` didn't drop the job entry. \
             Check `JobController::disown` (orkia-shell/src/job/mod.rs).",
            &related,
            session,
        );
    }
    stages.push("ps_after_disown".into());

    // Critical: disown must NOT have emitted a lifecycle:stopped (that
    // would mean the agent was killed). Allow `detached` from prior
    // Ctrl-Z but reject `stopped` increments.
    let lifecycle_count_after = count_lifecycle_events(session).await;
    let new_events: Vec<String> =
        recent_lifecycle_events_after(session, lifecycle_count_before).await;
    if new_events
        .iter()
        .any(|s| s.contains("\"event\":\"stopped\""))
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "no_stop_envelope_emitted",
            &HarnessError::assertion(
                "lifecycle:stopped seen after disown",
                AssertKind::Journal,
                format!(
                    "new events ({} → {}):\n{}",
                    lifecycle_count_before,
                    lifecycle_count_after,
                    new_events.join("\n")
                ),
            ),
            "disown should ONLY remove the job entry — it must NOT call kill or signal. \
             Check `handle_disown` (repl.rs:1693) for any signal()/stop() calls.",
            &related,
            session,
        );
    }
    stages.push("no_stop_envelope_emitted".into());

    pass_report(id, name, t0, stages)
}

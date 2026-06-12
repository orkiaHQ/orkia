// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F502 — ORKIA_SCHEDULED=1 invocation behaves as a crond fire.

use super::super::shared::*;
use super::S5_SCHED_RELATED;
use crate::report::FlowReport;
use orkia_e2e_harness::OrkiaSession;
use std::time::{Duration, Instant};

/// F502 — under `ORKIA_SCHEDULED=1` (own session group), a normal agent
/// invocation still follows the post-fire path: it runs to completion (or,
/// for approval-required agents, parks). Decouples "what Orkia does on a
/// fire" from "crond fires at the right time" (untestable, no mock clock).
pub(crate) async fn flow_f502(session: &mut OrkiaSession) -> FlowReport {
    let id = "F502-every-scheduled-fire";
    let name = "ORKIA_SCHEDULED invocation behaves as a crond fire (spawn or park)";
    let t0 = Instant::now();
    let mut stages = Vec::<String>::new();
    let related: Vec<String> = S5_SCHED_RELATED.iter().map(|s| s.to_string()).collect();

    if let Err(e) = boot_login(session).await {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "boot_login",
            &e,
            "See F101.",
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
            "If no spawn under ORKIA_SCHEDULED: the scheduled env broke normal dispatch. \
             If ORKIA_SCHEDULED isn't read at all, verify FlowEnv.extra_env injected it at boot.",
            &related,
            session,
        );
    }
    tokio::time::sleep(Duration::from_secs(3)).await; // script sleeps 2s + Exit
    if let Err(e) = session.force_reap().await {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "reap",
            &e,
            "force_reap failed.",
            &related,
            session,
        );
    }
    stages.push("spawn".into());

    let completed = session
        .journal()
        .count_envelopes_with(|e| {
            e.event_type() == Some("lifecycle") && e.event() == Some("completed")
        })
        .await
        .unwrap_or(0);
    let parked = session
        .shell()
        .map(|s| {
            std::fs::read_dir(s.data_dir.join("pending"))
                .map(|rd| {
                    rd.flatten()
                        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                        .count()
                })
                .unwrap_or(0)
        })
        .unwrap_or(0);

    if completed == 0 && parked == 0 {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "scheduled_behaves",
            "ASSERTION_FAILED",
            "scheduled fire produced neither a Completed lifecycle event nor a pending/*.json"
                .into(),
            "A scheduled invocation (ORKIA_SCHEDULED=1) should run the agent (lifecycle:completed) \
             or park it for approval (pending/*.json). Neither happened — the scheduled path may be a \
             silent no-op. Check the seal consumer's ORKIA_SCHEDULED routing (consumer.rs) and \
             pending.rs parking. If ORKIA_SCHEDULED isn't read, verify FlowEnv.extra_env injected it.",
            &related,
        );
    }
    stages.push("scheduled_behaves".into());

    pass_report(id, name, t0, stages)
}

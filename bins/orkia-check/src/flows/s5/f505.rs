// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F505 — `app usage` is premium-gated; OSS refuses cleanly.

use super::super::shared::*;
use super::S5_APP_RELATED;
use crate::report::FlowReport;
use orkia_e2e_harness::OrkiaSession;
use std::time::{Duration, Instant};

/// F505 — `app usage` is premium-gated; OSS refuses cleanly.
pub(crate) async fn flow_f505(session: &mut OrkiaSession) -> FlowReport {
    let id = "F505-app-usage-refusal";
    let name = "app usage refused cleanly in OSS (premium-gated)";
    let t0 = Instant::now();
    let mut stages = Vec::<String>::new();
    let related: Vec<String> = S5_APP_RELATED.iter().map(|s| s.to_string()).collect();

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

    if let Err(e) = session
        .run("app usage", "premium", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "app_usage_refused",
            &e,
            "If no refusal: app usage should be premium-gated in OSS (repl.rs:1503; free plan lacks \
             ForgeBuild → 'requires an Orkia premium plan'). If real usage data appeared, a premium \
             ForgeBuilder got wired into OSS.",
            &related,
            session,
        );
    }
    stages.push("app_usage_refused".into());

    if let Err(e) = session
        .run("ps", "none running", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "shell_responsive",
            &e,
            "If the shell hangs after the usage refusal: the error path may panic. Check the handler.",
            &related,
            session,
        );
    }
    stages.push("shell_responsive".into());

    pass_report(id, name, t0, stages)
}

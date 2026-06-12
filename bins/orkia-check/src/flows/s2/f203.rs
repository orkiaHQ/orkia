// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F203 — rfc forge noop OSS.

use std::time::{Duration, Instant};

use crate::report::FlowReport;
use orkia_e2e_harness::OrkiaSession;

use super::super::shared::*;

/// F203 — `rfc forge` on the OSS binary returns the premium-required error.
/// Smallest of the S2 flows; validates the open-core boundary.
pub(crate) async fn flow_f203(session: &mut OrkiaSession) -> FlowReport {
    let id = "F203-rfc-forge-noop-oss";
    let name = "rfc forge on OSS binary returns premium-required error gracefully";
    let t0 = Instant::now();
    let mut stages = Vec::<String>::new();
    let related: Vec<String> = S2_RELATED.iter().map(|s| s.to_string()).collect();

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

    // RFC create + cd to give forge a target.
    let slug = "test-forge-noop";
    let create_cmd =
        format!("rfc create {slug} --title 'Test forge noop' --project default-project");
    if let Err(e) = session
        .run(&create_cmd, slug, Duration::from_secs(10))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "rfc_create",
            &e,
            "If timeout: EDITOR env may not be `true`. See F003 hypothesis.",
            &related,
            session,
        );
    }
    stages.push("rfc_create".into());

    // `rfc forge <slug>` without --offline hits the capability gate.
    // The Free fixture session (plan=free from the backend) → `has_forge_capability`
    // returns false → error string contains "premium". Output: 'Forge build requires an Orkia premium plan...'
    let forge_cmd = format!("rfc forge {slug} --project default-project");
    if let Err(e) = session
        .run(&forge_cmd, "premium", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "rfc_forge_returns_premium",
            &e,
            "If 'premium' missing: \
             (1) Check `has_forge_capability` in orkia-shell/src/repl.rs — it should return false on plan=free. \
             (2) Check the error message in handle_rfc_forge:3824 — must say 'Forge build requires an Orkia premium plan'. \
             (3) If a real ForgeBuilder is wired: binary built with cloud feature — check OSS build flags.",
            &related,
            session,
        );
    }
    stages.push("rfc_forge_returns_premium".into());

    // After the error, the shell must still be responsive — proves the forge
    // path returned an Err, didn't panic.
    if let Err(e) = session
        .run("ps", "none running", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "shell_responsive_after_error",
            &e,
            "If shell unresponsive: handle_rfc_forge may have panicked. \
             Errors from the capability gate must propagate as Outcome::Error, not panic.",
            &related,
            session,
        );
    }
    stages.push("shell_responsive_after_error".into());

    pass_report(id, name, t0, stages)
}

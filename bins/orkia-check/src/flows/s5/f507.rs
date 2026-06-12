// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F507 — `contribute` refuses cleanly when the kernel daemon is absent.

use super::super::shared::*;
use crate::report::FlowReport;
use orkia_e2e_harness::OrkiaSession;
use std::time::{Duration, Instant};

/// F507 — `contribute` refuses cleanly when the kernel daemon is absent
/// (compose has no kernel). The real egress gate is proprietary.
pub(crate) async fn flow_f507(session: &mut OrkiaSession) -> FlowReport {
    let id = "F507-contribute-kernel-absent";
    let name = "contribute refused cleanly when kernel daemon absent";
    let t0 = Instant::now();
    let mut stages = Vec::<String>::new();
    let related: Vec<String> = ["contribute"].iter().map(|s| s.to_string()).collect();

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
        .run("contribute", "kernel not running", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "contribute_status",
            &e,
            "If no kernel-absent message: contribute should detect the missing kernel daemon \
             (contribute_builtins.rs:68). The compose stack runs no kernel, so this always hits \
             the 'kernel not running' branch.",
            &related,
            session,
        );
    }
    stages.push("contribute_status".into());

    if let Err(e) = session
        .run(
            "contribute on",
            "kernel not running",
            Duration::from_secs(5),
        )
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "contribute_on",
            &e,
            "If 'contribute on' succeeds without a kernel: it must refuse (contribute_builtins.rs:134). \
             Can't enable contribution when the consent daemon isn't running.",
            &related,
            session,
        );
    }
    stages.push("contribute_on".into());

    pass_report(id, name, t0, stages)
}

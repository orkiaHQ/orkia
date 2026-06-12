// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F503 — `app inspect` / `app perms` render a seeded manifest.

use super::super::shared::*;
use super::{S5_APP_RELATED, test_manifest};
use crate::report::FlowReport;
use orkia_e2e_harness::OrkiaSession;
use std::time::{Duration, Instant};

/// F503 — `app inspect` / `app perms` render a seeded manifest.
pub(crate) async fn flow_f503(session: &mut OrkiaSession) -> FlowReport {
    let id = "F503-app-inspect-perms";
    let name = "app inspect and perms render seeded manifest fields";
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

    if let Err(e) = session.seed_forge_app("test-app", test_manifest()) {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "seed_app",
            &e,
            "seed_forge_app failed — check it writes to data_dir/forge/<name>/manifest.toml \
             (where default_app_root reads).",
            &related,
            session,
        );
    }
    stages.push("seed_app".into());

    if let Err(e) = session
        .run("app inspect test-app", "0.1.0", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "app_inspect",
            &e,
            "If version missing: check inspect (handlers.rs:105) + manifest parse (manifest.rs). \
             If 'app not found': fixture seeded at the wrong path — must match discover.rs (default_app_root \
             = $HOME/.orkia/forge/<name>/manifest.toml).",
            &related,
            session,
        );
    }
    stages.push("app_inspect".into());

    if let Err(e) = session
        .run(
            "app perms test-app",
            "api.example.com",
            Duration::from_secs(5),
        )
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "app_perms",
            &e,
            "If the whitelisted domain isn't shown: check perms (handlers.rs:181) parsing the \
             [forge.permissions] network list.",
            &related,
            session,
        );
    }
    stages.push("app_perms".into());

    pass_report(id, name, t0, stages)
}

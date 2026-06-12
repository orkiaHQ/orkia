// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F501 — full `every` lifecycle against the isolated crontab spool.

use super::super::shared::*;
use super::S5_SCHED_RELATED;
use crate::report::FlowReport;
use orkia_e2e_harness::OrkiaSession;
use std::time::{Duration, Instant};

/// F501 — full `every` lifecycle against the isolated crontab spool.
/// create → list → pause → resume → remove. Proves cron-line generation,
/// tags, and the pause/resume toggle run for real while only the storage
/// backend is redirected.
pub(crate) async fn flow_f501(session: &mut OrkiaSession) -> FlowReport {
    let id = "F501-every-crud-roundtrip";
    let name = "every create/list/pause/resume/remove against isolated crontab spool";
    let t0 = Instant::now();
    let mut stages = Vec::<String>::new();
    let related: Vec<String> = S5_SCHED_RELATED.iter().map(|s| s.to_string()).collect();
    const SPOOL: &str = "test-crontab-spool";

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
        .run(
            r#"every "daily 18:00" @faye check the logs"#,
            "Scheduled",
            Duration::from_secs(5),
        )
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "every_create",
            &e,
            "If no '✓ Scheduled' line: check handle_every create (every.rs:147). \
             If 'crontab not available'/'not found': the shim isn't on PATH — check setup_crontab_shim \
             and that <home>/bin is prepended to PATH (session.rs try_start_shell).",
            &related,
            session,
        );
    }
    stages.push("every_create".into());

    // Critical isolation check: the entry landed in the sandbox spool.
    if let Err(e) = session.files().contains(SPOOL, "orkia:faye") {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "create_wrote_to_spool",
            &e,
            "If the spool lacks the 'orkia:faye' tag: the shim didn't receive the write, OR orkia \
             wrote to the real crontab. Verify Command::new(\"crontab\") resolved to the shim (PATH order) \
             and the shim honored ORKIA_TEST_CRONTAB_SPOOL.",
            &related,
            session,
        );
    }
    stages.push("create_wrote_to_spool".into());

    if let Err(e) = session
        .run("every list", "faye", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "every_list",
            &e,
            "If the entry isn't listed: check handle_every list (every.rs:228) and the shim's \
             `crontab -l` reading the spool.",
            &related,
            session,
        );
    }
    stages.push("every_list".into());

    if let Err(e) = session
        .run("every pause 1", "Paused", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "every_pause",
            &e,
            "If no '✓ Paused': check pause handling (every.rs:251).",
            &related,
            session,
        );
    }
    if let Err(e) = session.files().contains(SPOOL, "# PAUSED:") {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "every_pause",
            &e,
            "If no '# PAUSED:' prefix in spool: pause toggle didn't persist. Check crontab.rs:176-206 \
             (must prepend '# PAUSED: ' to the command line).",
            &related,
            session,
        );
    }
    stages.push("every_pause".into());

    if let Err(e) = session
        .run("every resume 1", "Resumed", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "every_resume",
            &e,
            "If no '✓ Resumed': check resume handling (every.rs:251).",
            &related,
            session,
        );
    }
    // File-level not_contains avoids the cumulative-screen trap (the earlier
    // paused `every list` still shows '(paused)' on screen).
    if let Err(e) = session.files().not_contains(SPOOL, "# PAUSED:") {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "every_resume",
            &e,
            "If '# PAUSED:' still in spool after resume: resume didn't strip the prefix. \
             Check crontab.rs:186-197.",
            &related,
            session,
        );
    }
    stages.push("every_resume".into());

    if let Err(e) = session
        .run("every remove 1", "Removed", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "every_remove",
            &e,
            "If no '✓ Removed': check remove handling (every.rs:235, crontab.rs:149-172).",
            &related,
            session,
        );
    }
    // After remove, list reports the empty marker (robust positive assertion
    // rather than not_contains on the cumulative screen).
    if let Err(e) = session
        .run("every list", "no scheduled jobs", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "every_remove",
            &e,
            "If list isn't empty after remove: removal didn't apply to the spool. \
             Check crontab.rs:149-172.",
            &related,
            session,
        );
    }
    stages.push("every_remove".into());

    pass_report(id, name, t0, stages)
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F002 — real backend login at boot, whoami shows the fixture identity.

use super::super::shared::*;
use crate::report::FlowReport;
use orkia_e2e_harness::{OrkiaSession, Plan};
use std::time::{Duration, Instant};

/// F002 — the harness logged the Free fixture account in for real against
/// the compose backend at boot (signed JWT persisted to the session file);
/// `whoami` must show that identity. No in-shell `login` stage: the
/// magic-link flow is interactive (email prompt) and can't complete
/// headless — the session is established before the shell boots.
pub(crate) async fn flow_f002(session: &mut OrkiaSession) -> FlowReport {
    let id = "F002-login-and-whoami";
    let name = "Real backend session loaded; whoami shows identity";
    let t0 = Instant::now();
    let mut stages = Vec::<String>::new();

    if !session.has_shell() {
        return fail_report(
            id,
            name,
            t0,
            &stages,
            "boot",
            "INFRA_UNREACHABLE",
            "orkia shell not booted in this session".into(),
            "Set ORKIA_TEST_BIN or build the `orkia-cli` crate.",
        );
    }
    if let Err(e) = session
        .wait_for("\x1b]133;A", Duration::from_secs(10))
        .await
    {
        return fail_report(
            id,
            name,
            t0,
            &stages,
            "boot",
            "TIMEOUT",
            format!("{e}"),
            "Initial prompt never reached OSC 133;A.",
        );
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    stages.push("boot".into());

    // Stage: whoami — must echo the Free fixture email the harness logged
    // in as at boot (`Plan::Free.fixture_email()`).
    if let Err(e) = session
        .run("whoami", Plan::Free.fixture_email(), Duration::from_secs(5))
        .await
    {
        return fail_report(
            id,
            name,
            t0,
            &stages,
            "whoami",
            &classify(&e),
            format!("{e}"),
            "If 'not signed in': the boot-time real login failed — check `login::login_to_session_file` \
             and that the compose backend is up (ORKIA_BACKEND_URL). If email differs: check \
             `render_whoami` in orkia-shell/src/auth_builtins.rs (header '@<user> · <email>').",
        );
    }
    stages.push("whoami".into());

    FlowReport {
        id: id.into(),
        name: name.into(),
        status: crate::report::FlowStatus::Pass,
        duration_ms: elapsed_ms(t0),
        env_group: String::new(),
        stages_completed: stages,
        stage_failed: None,
        failure: None,
    }
}

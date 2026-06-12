// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F001 — boot shell, run ps, assert no agents.

use super::super::shared::*;
use crate::report::{FailureDetail, FlowReport, FlowStatus};
use orkia_e2e_harness::OrkiaSession;
use std::time::{Duration, Instant};

/// F001 — boot shell, type `ps`, expect "none running" output, expect
/// no `AgentSpawn` journal envelopes.
pub(crate) async fn flow_f001(session: &mut OrkiaSession) -> FlowReport {
    let id = "F001-boot-and-ps";
    let name = "Boot shell and run ps with no agents";
    let t0 = Instant::now();
    let mut stages = Vec::<String>::new();

    macro_rules! fail {
        ($stage:literal, $code:expr, $msg:expr, $hyp:expr) => {{
            stages.push($stage.into());
            return FlowReport {
                id: id.into(),
                name: name.into(),
                status: FlowStatus::Fail,
                duration_ms: elapsed_ms(t0),
                env_group: String::new(),
                stages_completed: stages.clone(),
                stage_failed: Some($stage.into()),
                failure: Some(FailureDetail {
                    code: $code.into(),
                    message: $msg,
                    expected: String::new(),
                    actual: String::new(),
                    hypothesis: $hyp.into(),
                    logs_at: String::new(),
                    rendered_output_excerpt: String::new(),
                    related_specs: vec!["shell".into()],
                }),
            };
        }};
    }

    // Stage: boot — the shell is brought up by start_compose.
    if !session.has_shell() {
        fail!(
            "boot",
            "INFRA_UNREACHABLE",
            "orkia shell not booted in this session".into(),
            "Set ORKIA_TEST_BIN or build the `orkia-cli` crate (binary name `orkia`)."
        );
    }
    stages.push("boot".into());

    // Wait for the initial prompt to fully render. Orkia emits an OSC
    // 133;A prompt mark when the prompt is ready for input; that's
    // the unambiguous "you may type now" signal.
    if let Err(e) = session
        .wait_for("\x1b]133;A", Duration::from_secs(10))
        .await
    {
        let msg = format!("{e}");
        fail!(
            "boot",
            "TIMEOUT",
            msg,
            "If OSC 133;A absent: shell may have crashed at boot — check stderr from the orkia binary. \
             OSC mark is emitted by `crate::prompt::draw_prompt` in orkia-shell — verify the constant still uses 133;A."
        );
    }
    // Give ratatui one more frame to settle after the prompt mark.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Stage: run_ps
    if let Err(e) = session
        .run("ps", "none running", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "run_ps",
            &e,
            "If 'none running' marker absent: check `push_agents_section` in orkia-builtin/src/ps.rs — the empty-state string was ' AGENTS — none running'. \
             If shell never produced output: check the PTY input drip-feed timing in `orkia-test-harness::PtyDriver::type_str` (10ms per char).",
            &["shell".to_string()],
            session,
        );
    }
    stages.push("run_ps".into());

    // Stage: assert_empty — journal must show no AgentSpawn envelopes.
    if let Err(e) = session.journal().envelope_count("AgentSpawn", 0).await {
        let msg = format!("{e}");
        fail!(
            "assert_empty",
            "ASSERTION_FAILED",
            msg,
            "If any AgentSpawn-type event seen: a background mechanism is spawning agents on shell boot. \
             Check the journal for what fired — `ps` should be read-only."
        );
    }
    stages.push("assert_empty".into());

    FlowReport {
        id: id.into(),
        name: name.into(),
        status: FlowStatus::Pass,
        duration_ms: elapsed_ms(t0),
        env_group: String::new(),
        stages_completed: stages,
        stage_failed: None,
        failure: None,
    }
}

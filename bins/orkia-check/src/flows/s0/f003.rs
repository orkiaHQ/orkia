// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F003 — full RFC lifecycle: create → promote → complete, verify SEAL v1.

use super::super::shared::*;
use crate::report::{FailureDetail, FlowReport, FlowStatus};
use orkia_e2e_harness::OrkiaSession;
use std::time::{Duration, Instant};

/// F003 — full RFC lifecycle: create → promote → complete, then assert
/// a SEAL v1 document was assembled at the conventional path. Requires
/// `ORKIA_TEST_BIN` to point at a build that wires the SEAL assembler via
/// `with_seal_assembler`; the default public `orkia` binary leaves it
/// unwired, so `rfc complete` does NOT emit the assembler message and no
/// file is written (the flow will surface this clearly).
pub(crate) async fn flow_f003(session: &mut OrkiaSession) -> FlowReport {
    let id = "F003-rfc-complete-produces-seal-v1";
    let name = "Create RFC, promote, complete; verify SEAL v1 document";
    let t0 = Instant::now();
    let mut stages = Vec::<String>::new();
    let related = vec!["seal".to_string()];

    if !session.has_shell() {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "boot",
            "INFRA_UNREACHABLE",
            "orkia shell not booted".into(),
            "Set ORKIA_TEST_BIN, built with --features seal-v1-assembler.",
            &related,
        );
    }
    if let Err(e) = session
        .wait_for("\x1b]133;A", Duration::from_secs(10))
        .await
    {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "boot",
            "TIMEOUT",
            format!("{e}"),
            "Initial prompt never reached OSC 133;A.",
            &related,
        );
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    stages.push("boot".into());

    // The harness logged the fixture account in for real at boot; the
    // session is already loaded. No in-shell `login` (interactive
    // magic-link can't complete headless). F003 does not depend on the
    // session anyway.

    // rfc create — emits the rfc.create event; spawns $EDITOR (set to
    // `true` in harness env so it no-ops). We can't assert on output
    // because the create handler emits no visible "created" string.
    let create_cmd = "rfc create test-rfc-001 --title E2E --project default-project";
    if let Err(e) = session
        .run(create_cmd, "test-rfc-001", Duration::from_secs(10))
        .await
    {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "rfc_create",
            &classify(&e),
            format!("{e}"),
            "If timeout: EDITOR env may not be `true` — check `try_start_shell` env injection. \
             Also check `rfc::create` in orkia-builtin/src/rfc.rs and `spawn_editor_and_seal` in orkia-shell/src/repl.rs.",
            &related,
        );
    }
    stages.push("rfc_create".into());

    // rfc cd — sets rfc_scope so subsequent custom events get tagged
    if let Err(e) = session
        .run(
            "rfc cd test-rfc-001 --project default-project",
            "test-rfc-001",
            Duration::from_secs(5),
        )
        .await
    {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "rfc_cd",
            &classify(&e),
            format!("{e}"),
            "If timeout: check `handle_rfc_cd` in orkia-shell/src/repl.rs and the prompt rendering of `rfc_scope`.",
            &related,
        );
    }
    stages.push("rfc_cd".into());

    // rfc ask — opens a clarification (decision row). Required because
    // newly-created RFCs land in DraftEmpty and `promote` is gated on
    // DraftActive, which is reached by resolving at least one clarification.
    if let Err(e) = session
        .run(
            "rfc ask test-rfc-001 --q anything --rationale e2e --project default-project",
            "clarification",
            Duration::from_secs(5),
        )
        .await
    {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "rfc_ask",
            &classify(&e),
            format!("{e}"),
            "If timeout: check `handle_rfc_ask` output formatting in orkia-shell/src/repl.rs. \
             The clarification format is `clarification <id> opened on rfc <slug>`.",
            &related,
        );
    }
    stages.push("rfc_ask".into());

    // Pull the decision-id out of the on-disk decision JSONL.
    let decision_id = match find_decision_id(session, "default-project", "test-rfc-001") {
        Ok(id) => id,
        Err(e) => {
            return fail_with(
                id,
                name,
                t0,
                &stages,
                "rfc_ask",
                "RUNTIME_ERROR",
                format!("could not extract decision id: {e}"),
                "Check `RfcStore::decision_path` and `append_decision` in orkia-rfc-core/src/store.rs. \
             If field name changed (was `id`): update `find_decision_id` in flows.rs.",
                &related,
            );
        }
    };

    // rfc resolve — DraftEmpty -> DraftActive once a clarification resolves.
    let resolve_cmd =
        format!("rfc resolve {decision_id} test-rfc-001 --answer yes --project default-project");
    if let Err(e) = session
        .run(&resolve_cmd, "resolved", Duration::from_secs(5))
        .await
    {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "rfc_resolve",
            &classify(&e),
            format!("{e}"),
            "If timeout: check `handle_rfc_resolve` output in orkia-shell/src/repl.rs (expected: 'resolved <did> on rfc <slug>').",
            &related,
        );
    }
    stages.push("rfc_resolve".into());

    // rfc promote — DraftActive -> Active. Required before complete.
    if let Err(e) = session
        .run(
            "rfc promote test-rfc-001 --project default-project --yes",
            "Active",
            Duration::from_secs(5),
        )
        .await
    {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "rfc_promote",
            &classify(&e),
            format!("{e}"),
            "If `state DraftActive` error: rfc_resolve didn't flip state — check `resolve_clarification` in orkia-rfc-state. \
             If `--yes` rejected: check `has_confirm_flag` in orkia-builtin/src/rfc.rs. \
             State matrix lives in orkia-rfc-core/src/matrix.rs.",
            &related,
        );
    }
    stages.push("rfc_promote".into());

    // rfc complete — Active -> Completed. With seal-v1-assembler feature,
    // the handler emits " | SEAL v1 document: <path> (N events)".
    if let Err(e) = session
        .run(
            "rfc complete test-rfc-001 --project default-project --yes",
            "SEAL v1 document",
            Duration::from_secs(15),
        )
        .await
    {
        let screen = session
            .shell()
            .map(|s| s.process.pty.screen_text())
            .unwrap_or_default();
        let raw = session
            .shell()
            .map(|s| s.process.pty.raw_text())
            .unwrap_or_default();
        let excerpt = format!(
            "--- screen ---\n{}\n--- raw (last 2000) ---\n{}",
            screen,
            raw.replace('\x1b', "\\e")
                .chars()
                .rev()
                .take(2000)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        );
        stages.push("rfc_complete".into());
        return FlowReport {
            id: id.into(), name: name.into(),
            status: FlowStatus::Fail, duration_ms: elapsed_ms(t0),
            env_group: String::new(),
            stages_completed: stages.clone(), stage_failed: Some("rfc_complete".into()),
            failure: Some(FailureDetail {
                code: classify(&e), message: format!("{e}"),
                expected: "screen contains 'SEAL v1 document'".into(),
                actual: String::new(),
                hypothesis: "If marker absent: ORKIA_TEST_BIN is a build without a wired SEAL assembler. \
                             If marker text differs: check `maybe_assemble_seal_v1` in orkia-shell/src/repl/forge.rs.".into(),
                logs_at: String::new(),
                rendered_output_excerpt: excerpt,
                related_specs: related.clone(),
            }),
        };
    }
    stages.push("rfc_complete".into());

    // assert: SEAL v1 file exists at <data_dir>/seal-v1/test-rfc-001-*.seal-completed.jsonl
    // (suffix carries the ClosureReason; Completed → "seal-completed").
    if let Err(e) = session
        .files()
        .matches_glob("seal-v1/test-rfc-001-*.seal-completed.jsonl", 1)
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "assert_seal",
            &e,
            "If 0 matches: check `orkia_seal_assembler::assembler::assemble_rfc_seal_v1` write_path. \
             If non-zero but different name: check ClosureReason → suffix mapping in `lib.rs::file_stem`.",
            &related,
            session,
        );
    }
    stages.push("assert_seal".into());

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

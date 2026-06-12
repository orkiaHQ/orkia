// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F202 — rfc abandon SEAL v1.

use std::time::{Duration, Instant};

use crate::report::FlowReport;
use orkia_e2e_harness::OrkiaSession;

use super::super::shared::*;

/// F202 — `rfc abandon` produces a SEAL v1 document with `abandoned`
/// closure reason. Differentiates abandon from completed in the final
/// document name + footer.
pub(crate) async fn flow_f202(session: &mut OrkiaSession) -> FlowReport {
    let id = "F202-rfc-abandon-seal-v1";
    let name = "Abandon RFC produces SEAL v1 document with `abandoned` closure";
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
            "See F101.",
            &related,
            session,
        );
    }
    stages.push("boot_login".into());

    let slug = "test-abandon-rfc";
    let proj = "--project default-project";

    if let Err(e) = session
        .run(
            &format!("rfc create {slug} --title 'To be abandoned' {proj}"),
            slug,
            Duration::from_secs(10),
        )
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "rfc_create",
            &e,
            "See F003.",
            &related,
            session,
        );
    }
    if let Err(e) = session
        .run(
            &format!("rfc cd {slug} {proj}"),
            slug,
            Duration::from_secs(5),
        )
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "rfc_cd",
            &e,
            "See F003.",
            &related,
            session,
        );
    }

    // Abandon requires DraftActive or Active (state.rs:242 test). From
    // DraftEmpty (the post-`rfc create` state) abandon errors. Do one
    // ask+resolve cycle to reach DraftActive — that's enough activity
    // for the assembled SEAL to be non-trivial.
    let ask = format!("rfc ask {slug} --q anything --rationale e2e {proj}");
    if let Err(e) = session
        .run(&ask, "clarification", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "emit_events",
            &e,
            "rfc ask emission failed; see F201 hypothesis.",
            &related,
            session,
        );
    }
    let did = match find_decision_id(session, "default-project", slug) {
        Ok(d) => d,
        Err(e) => {
            return fail_with(
                id,
                name,
                t0,
                &stages,
                "emit_events",
                "RUNTIME_ERROR",
                format!("decision-id extract: {e}"),
                "See F003.",
                &related,
            );
        }
    };
    let resolve_cmd = format!("rfc resolve {did} {slug} --answer yes {proj}");
    if let Err(e) = session
        .run(&resolve_cmd, "resolved", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "emit_events",
            &e,
            "rfc resolve failed; see F003.",
            &related,
            session,
        );
    }
    stages.push("emit_events".into());

    // Now state is DraftActive — abandon is allowed.
    let cmd = format!("rfc abandon {slug} --reason no_longer_needed --yes {proj}");
    if let Err(e) = session
        .run(&cmd, "Abandoned", Duration::from_secs(15))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "rfc_abandon",
            &e,
            "If timeout: state may not allow abandon. Check matrix.rs — abandon allowed only \
             from DraftActive or Active (NOT DraftEmpty). \
             If 'wrong state' error: previous ask+resolve didn't flip to DraftActive.",
            &related,
            session,
        );
    }
    stages.push("rfc_abandon".into());

    // Assert the SEAL v1 file exists at the abandon path.
    let pattern = format!("seal-v1/{slug}-*.seal-abandoned.jsonl");
    if let Err(e) = session.files().matches_glob(&pattern, 1) {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "assert_seal_v1_abandoned_file",
            &e,
            "If 0 matches: maybe_assemble_seal_v1 didn't trigger on Abandon, OR the suffix differs. \
             Check `assembler::file_stem` ClosureReason::Abandoned branch — suffix should be 'seal-abandoned'.",
            &related,
            session,
        );
    }
    stages.push("assert_seal_v1_abandoned_file".into());

    // Verify the document. `rfc seal <slug> --verify` outputs "VALID (...)".
    let verify_cmd = format!("rfc seal {slug} --verify {proj}");
    if let Err(e) = session
        .run(&verify_cmd, "VALID", Duration::from_secs(10))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "assert_seal_verifies",
            &e,
            "If 'VALID' missing and 'INVALID' present: signature/chain verification failed on a freshly-built doc. \
             Check that assembler used the same workspace key for signing AND that verify reads it the same way.",
            &related,
            session,
        );
    }
    stages.push("assert_seal_verifies".into());

    pass_report(id, name, t0, stages)
}

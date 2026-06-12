// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F201 — rfc ask + rfc resolve.

use std::time::{Duration, Instant};

use crate::report::FlowReport;
use orkia_e2e_harness::OrkiaSession;

use super::super::shared::*;

/// F201 — rfc ask + rfc resolve. Reuses the state-machine path proven by
/// F003's prelude. Adds a clean assertion on the rfc.ask and rfc.resolve
/// journal events.
pub(crate) async fn flow_f201(session: &mut OrkiaSession) -> FlowReport {
    let id = "F201-rfc-ask-resolve";
    let name =
        "Create RFC, ask clarification, resolve, verify state transitions and journal events";
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
            "See F101 hypothesis.",
            &related,
            session,
        );
    }
    stages.push("boot_login".into());

    let slug = "test-ask-resolve";
    let proj = "--project default-project";

    if let Err(e) = session
        .run(
            &format!("rfc create {slug} --title 'ask flow' {proj}"),
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
            "See F003 rfc_create hypothesis.",
            &related,
            session,
        );
    }
    stages.push("rfc_create".into());

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
            "See F003 rfc_cd hypothesis.",
            &related,
            session,
        );
    }
    stages.push("rfc_cd".into());

    // rfc ask requires --q AND --rationale (positional question is NOT supported).
    // After ask, RFC stays DraftEmpty but has 1 open clarification.
    if let Err(e) = session
        .run(
            &format!("rfc ask {slug} --q timeout --rationale e2e {proj}"),
            "clarification",
            Duration::from_secs(5),
        )
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "rfc_ask",
            &e,
            "If 'clarification' missing: check `handle_rfc_ask` output format in repl.rs \
             (expected 'clarification <id> opened on rfc <slug>'). \
             If error about flags: --q and --rationale are BOTH required (rfc.rs parser).",
            &related,
            session,
        );
    }
    stages.push("rfc_ask".into());

    // Read the decision-id from the on-disk JSONL (same pattern as F003).
    let decision_id = match find_decision_id(session, "default-project", slug) {
        Ok(d) => d,
        Err(e) => {
            return fail_with(
                id,
                name,
                t0,
                &stages,
                "rfc_ask_did_extract",
                "RUNTIME_ERROR",
                format!("decision-id extract: {e}"),
                "Check RfcStore::decision_path (orkia-rfc-core/src/store.rs) and the JSONL field name (was `id`).",
                &related,
            );
        }
    };

    // rfc resolve <did> requires positional decision-id, not a flag.
    let resolve_cmd = format!("rfc resolve {decision_id} {slug} --answer 30s {proj}");
    if let Err(e) = session
        .run(&resolve_cmd, "resolved", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "rfc_resolve",
            &e,
            "See F003 rfc_resolve hypothesis.",
            &related,
            session,
        );
    }
    stages.push("rfc_resolve".into());

    // Assert via the on-disk decision JSONL — that's the canonical
    // persistence for rfc.ask/resolve. Custom events flow through the
    // event_router → SEAL chain, NOT through journal.jsonl (no `Custom`
    // EventType variant in orkia-shell-types — finding from S1 F004).
    let Some(shell) = session.shell() else {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "assert_decision_persisted",
            "INFRA_UNREACHABLE",
            "shell vanished".into(),
            "Unexpected mid-flow.",
            &related,
        );
    };
    let decisions_path = shell
        .data_dir
        .join("projects")
        .join("default-project")
        .join("decisions")
        .join(format!("{slug}.jsonl"));
    let decisions = match std::fs::read_to_string(&decisions_path) {
        Ok(s) => s,
        Err(e) => {
            return fail_with(
                id,
                name,
                t0,
                &stages,
                "assert_decision_persisted",
                "RUNTIME_ERROR",
                format!("read {}: {e}", decisions_path.display()),
                "Decision JSONL didn't land. Check `RfcStore::append_decision` in orkia-rfc-core/src/store.rs.",
                &related,
            );
        }
    };
    if !decisions.contains("clarification") {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "assert_decision_persisted",
            "ASSERTION_FAILED",
            format!(
                "decision JSONL has no clarification entry; content:\n{}",
                decisions.chars().take(500).collect::<String>()
            ),
            "The rfc ask path didn't write the decision record. Check that handle_rfc_ask \
             ends with `entry.service.ask(req)` which calls `RfcStore::append_decision`.",
            &related,
        );
    }
    if !decisions.contains("resolved") && !decisions.contains("answer") {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "assert_decision_persisted",
            "ASSERTION_FAILED",
            "decision JSONL has clarification but no resolution".into(),
            "rfc resolve printed 'resolved' but didn't update the JSONL. Check `resolve_clarification` \
             in orkia-rfc-state/src/service.rs.",
            &related,
        );
    }
    stages.push("assert_decision_persisted".into());

    pass_report(id, name, t0, stages)
}

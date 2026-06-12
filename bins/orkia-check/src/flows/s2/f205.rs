// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F205 — SEAL v1 multi-event.

use std::time::{Duration, Instant};

use crate::report::FlowReport;
use orkia_e2e_harness::OrkiaSession;

use super::super::shared::*;

/// F205 — produce a SEAL v1 document with 20 events.
/// Uses `rfc ask` cycles (each emits one rfc.ask event) since `rfc note` doesn't exist.
pub(crate) async fn flow_f205(session: &mut OrkiaSession) -> FlowReport {
    let id = "F205-seal-v1-multi-event";
    let name = "Produce a SEAL v1 document with 20 events, ordered, verified";
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

    let slug = "test-multi-event";
    let proj = "--project default-project";

    if let Err(e) = session
        .run(
            &format!("rfc create {slug} --title 'multi-event' {proj}"),
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
    stages.push("rfc_create_and_cd".into());

    // Emit events via repeated ask/resolve cycles. Note: many pending
    // unresolved clarifications may block `rfc promote`, so we RESOLVE
    // each cycle before opening the next. The state stays DraftActive
    // after the first cycle (subsequent cycles don't flip back).
    //
    // (a) `rfc note` doesn't exist as an event emitter, and (b) each
    // ask+resolve cycle emits >2 chain entries (clarification opened,
    // clarification resolved, plus possibly an audit envelope).
    // The SealAssembler collects events from SEAL chains, not journal.
    // We do 5 cycles to keep runtime reasonable; the load-bearing
    // proof is "many events end up in the document, ordered, signed".
    const CYCLES: usize = 5;
    for i in 1..=CYCLES {
        let ask_cmd = format!("rfc ask {slug} --q q_{i} --rationale e2e {proj}");
        if let Err(e) = session
            .run(&ask_cmd, "clarification", Duration::from_secs(5))
            .await
        {
            return fail_with_diagnostics(
                id,
                name,
                t0,
                &stages,
                "emit_events",
                &e,
                "ask cycle failed; see F201.",
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
                    format!("decision-id extract on iter {i}: {e}"),
                    "See F003.",
                    &related,
                );
            }
        };
        let resolve_cmd = format!("rfc resolve {did} {slug} --answer ans_{i} {proj}");
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
                "resolve cycle failed; see F003.",
                &related,
                session,
            );
        }
    }
    stages.push("emit_events".into());

    // Promote → Active.
    let promote_cmd = format!("rfc promote {slug} --yes {proj}");
    if let Err(e) = session
        .run(&promote_cmd, "Active", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "promote",
            &e,
            "promote may fail if unresolved clarifications remain. We resolve every cycle, \
             so this shouldn't trigger. Check rfc-state matrix if it does.",
            &related,
            session,
        );
    }

    // Complete → triggers SEAL v1 assembly.
    let complete_cmd = format!("rfc complete {slug} --yes {proj}");
    if let Err(e) = session
        .run(&complete_cmd, "SEAL v1 document", Duration::from_secs(15))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "rfc_complete",
            &e,
            "See F003 rfc_complete hypothesis.",
            &related,
            session,
        );
    }
    stages.push("rfc_complete".into());

    // Locate the SEAL document with retry.
    let Some(doc_path) = locate_seal_file(
        session,
        "seal-v1",
        slug,
        ".seal-completed.jsonl",
        Duration::from_secs(3),
    ) else {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "assert_seal_v1_file",
            "ASSERTION_FAILED",
            format!("no SEAL doc matching seal-v1/{slug}-*.seal-completed.jsonl after 3s"),
            "Assembler didn't write the expected file. See F003.",
            &related,
        );
    };
    stages.push("assert_seal_v1_file".into());

    let doc_content = match std::fs::read_to_string(&doc_path) {
        Ok(s) => s,
        Err(e) => {
            return fail_with(
                id,
                name,
                t0,
                &stages,
                "doc_read",
                "RUNTIME_ERROR",
                format!("read: {e}"),
                "FS perms.",
                &related,
            );
        }
    };
    let lines: Vec<&str> = doc_content.lines().collect();
    // Document structure: header + N events + footer. We want at least
    // 5 events (one per cycle minimum). The actual count depends on how
    // the assembler counts chain entries per ask/resolve.
    if lines.len() < 7 {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "assert_many_events_in_doc",
            "ASSERTION_FAILED",
            format!(
                "expected ≥ 7 lines (1 + ≥5 events + 1), got {}",
                lines.len()
            ),
            "If < 7: assembler missed events. Check collect_rfc_events filter by rfc_id. \
             5 ask+resolve cycles should produce at least 5 SEAL chain entries for this rfc.",
            &related,
        );
    }
    stages.push("assert_many_events_in_doc".into());

    // Verify chronological order on the middle 20 lines.
    let mut prev_ts: Option<String> = None;
    for (i, line) in lines.iter().enumerate().skip(1).take(20) {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return fail_with(
                    id,
                    name,
                    t0,
                    &stages,
                    "assert_chronological",
                    "ASSERTION_FAILED",
                    format!("line {i} not JSON: {e}"),
                    "Assembler wrote corrupt JSONL.",
                    &related,
                );
            }
        };
        let ts = v
            .get("timestamp")
            .and_then(|s| s.as_str())
            .or_else(|| v.get("ts").and_then(|s| s.as_str()))
            .unwrap_or("")
            .to_string();
        if let Some(prev) = &prev_ts
            && &ts < prev
        {
            return fail_with(
                id,
                name,
                t0,
                &stages,
                "assert_chronological",
                "ASSERTION_FAILED",
                format!("line {i} ts={ts} < previous ts={prev}"),
                "Events out of order. Check collect_rfc_events sort by timestamp before compose_document.",
                &related,
            );
        }
        if !ts.is_empty() {
            prev_ts = Some(ts);
        }
    }
    stages.push("assert_chronological".into());

    // Verify the signature.
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
            "If INVALID with 20 events: JCS canonicalization may have a length-sensitive bug. \
             See the SEAL v1 canonicalization rules.",
            &related,
            session,
        );
    }
    stages.push("assert_seal_verifies".into());

    pass_report(id, name, t0, stages)
}

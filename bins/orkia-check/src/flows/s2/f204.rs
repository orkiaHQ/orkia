// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F204 — SEAL v1 tampering.

use std::time::{Duration, Instant};

use crate::report::FlowReport;
use orkia_e2e_harness::OrkiaSession;

use super::super::shared::*;

/// F204 — flip one byte in the SEAL v1 JSONL and confirm `--verify` fails.
pub(crate) async fn flow_f204(session: &mut OrkiaSession) -> FlowReport {
    let id = "F204-seal-v1-tampering";
    let name = "Modify one byte in a SEAL v1 document, verify fails";
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

    // Prelude: build a complete RFC → SEAL v1. Reuse the F003 sequence.
    let slug = "test-tamper";
    let proj = "--project default-project";
    if let Err(e) = build_complete_rfc(session, slug, proj).await {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "prelude_build_rfc",
            &e,
            "Prelude (create→ask→resolve→promote→complete) failed; see F003 stage hypotheses.",
            &related,
            session,
        );
    }
    stages.push("prelude_build_rfc".into());

    // Find the SEAL doc. Retry up to 3s — the assembler may flush
    // slightly after the "SEAL v1 document" marker.
    let path = match locate_seal_file(
        session,
        "seal-v1",
        slug,
        ".seal-completed.jsonl",
        Duration::from_secs(3),
    ) {
        Some(p) => p,
        None => {
            return fail_with(
                id,
                name,
                t0,
                &stages,
                "locate_seal_doc",
                "ASSERTION_FAILED",
                format!("no SEAL doc matching seal-v1/{slug}-*.seal-completed.jsonl after 3s"),
                "Prelude marker appeared but file didn't materialize. \
             Check that assemble_rfc_seal_v1 fsync's before returning.",
                &related,
            );
        }
    };

    // Verify initially.
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
            "verify_passes_initially",
            &e,
            "If VALID missing on a freshly-built doc: assembler produced an invalid signature or chain hash. \
             Check orkia-seal-assembler::compose_document.",
            &related,
            session,
        );
    }
    stages.push("verify_passes_initially".into());

    // Tamper: flip a single byte in an event line. Find the FIRST event line
    // (line 2 — line 1 is the header) and flip an ASCII letter inside a quoted
    // string. This keeps the JSONL parseable while invalidating the chain hash.
    let content = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            return fail_with(
                id,
                name,
                t0,
                &stages,
                "tamper_read",
                "RUNTIME_ERROR",
                format!("read seal doc: {e}"),
                "Check FS perms on sandbox.",
                &related,
            );
        }
    };
    let mut bytes = content.clone();
    // Find line 2 (first event). Locate the second '\n'.
    let line_break_1 = bytes.iter().position(|&b| b == b'\n');
    let Some(lb1) = line_break_1 else {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "tamper_locate",
            "ASSERTION_FAILED",
            "seal doc has no newline; not multi-line JSONL".into(),
            "Check assembler output format: must be NDJSON, one envelope per line.",
            &related,
        );
    };
    // Walk forward from after the first newline; find an ASCII letter that's not
    // inside a JSON key (heuristic: skip first 20 bytes which are usually `{"id":"...`).
    let event_start = lb1 + 1;
    let target_byte = bytes[event_start..]
        .iter()
        .enumerate()
        .find(|&(idx, &b)| idx > 20 && b.is_ascii_lowercase() && b != b'\\')
        .map(|(idx, _)| event_start + idx);
    let Some(flip_at) = target_byte else {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "tamper_locate",
            "ASSERTION_FAILED",
            "no flippable byte found in event line".into(),
            "Event line is too short or has no lowercase letters past offset 20. Adjust the heuristic.",
            &related,
        );
    };
    bytes[flip_at] = if bytes[flip_at] == b'z' {
        b'a'
    } else {
        bytes[flip_at] + 1
    };
    if let Err(e) = std::fs::write(&path, &bytes) {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "tamper_write",
            "RUNTIME_ERROR",
            format!("write back: {e}"),
            "Check FS perms.",
            &related,
        );
    }
    stages.push("tamper_one_byte".into());

    // Re-verify — must fail. The actual error string from orkia is
    // `verification failed: <reason>` (lowercase, see handle_rfc_seal_cli
    // → VerifyOutcome::Invalid arm — output includes "INVALID (reason)"
    // OR an Err that propagates as "verification failed").
    if let Err(e) = session
        .run(&verify_cmd, "verification failed", Duration::from_secs(10))
        .await
    {
        // Try the other casing — VerifyOutcome::Invalid path prints "INVALID".
        if session
            .run(&verify_cmd, "INVALID", Duration::from_secs(5))
            .await
            .is_err()
        {
            return fail_with_diagnostics(
                id,
                name,
                t0,
                &stages,
                "verify_fails_after_tampering",
                &e,
                "If neither 'verification failed' nor 'INVALID' appeared: the verifier didn't catch \
                 the tampering. Check `verify_seal_v1_file` in orkia-audit::seal_v1 — must recompute \
                 chain hash AND verify signature, AND schema-check each event (which is what catches \
                 our byte flip turning a known field name into an unknown one).",
                &related,
                session,
            );
        }
    }
    stages.push("verify_fails_after_tampering".into());

    pass_report(id, name, t0, stages)
}

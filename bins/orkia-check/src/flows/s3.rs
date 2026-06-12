// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! S2.5 + S3-V1 flows (F206, F301).
//!
//! Audit findings baked in:
//!   * SEAL footer field is `events_count` (top-level); the footer signature
//!     COVERS it (seal_v1.rs:2425), so decrementing it fails SIGNATURE
//!     verification — no crypto gap.
//!   * OSS pipeline refusal: "@a | @b requires Orkia Team. See https://orkia.dev/team".

use super::shared::*;
use crate::report::FlowReport;
use orkia_e2e_harness::{OrkiaSession, Plan};
use std::time::{Duration, Instant};

/// F206 — decrement `events_count` in the SEAL footer (a value inside a
/// known field, JSON stays valid) and confirm `--verify` rejects it.
/// Complements F204 (which proves the *schema* path catches structural
/// tampering); F206 proves the *signature* path catches value tampering.
pub(crate) async fn flow_f206(session: &mut OrkiaSession) -> FlowReport {
    let id = "F206-seal-v1-value-tampering";
    let name = "Decrement events_count in footer; verify detects via signature failure";
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

    let slug = "test-value-tamper";
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

    // Verify it's valid first.
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
            "Freshly-built doc must verify VALID. If not, the assembler's signature/chain is broken.",
            &related,
            session,
        );
    }
    stages.push("verify_passes_initially".into());

    // Locate the doc (retry helper from S2).
    let Some(path) = locate_seal_file(
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
            "locate_seal_doc",
            "ASSERTION_FAILED",
            format!("no SEAL doc matching seal-v1/{slug}-*.seal-completed.jsonl"),
            "Prelude completed but no file. See F003.",
            &related,
        );
    };

    // Tamper: decrement `events_count` in the footer (last line). The
    // footer is `{"type":"SealFooter", ..., "events_count":N, ...}`.
    // JSON stays valid; only a numeric value changes. The footer
    // signature covers events_count so verification must fail.
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return fail_with(
                id,
                name,
                t0,
                &stages,
                "tamper_read",
                "RUNTIME_ERROR",
                format!("read: {e}"),
                "FS perms.",
                &related,
            );
        }
    };
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() < 3 {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "tamper",
            "ASSERTION_FAILED",
            format!(
                "doc has {} lines, need ≥3 (header+event+footer)",
                lines.len()
            ),
            "Prelude didn't emit enough events. Check ask/resolve event emission.",
            &related,
        );
    }
    let footer_line = lines[lines.len() - 1];
    let mut footer: serde_json::Value = match serde_json::from_str(footer_line) {
        Ok(v) => v,
        Err(e) => {
            return fail_with(
                id,
                name,
                t0,
                &stages,
                "tamper",
                "RUNTIME_ERROR",
                format!("footer parse: {e}"),
                "Footer isn't valid JSONL.",
                &related,
            );
        }
    };
    let original = footer.get("events_count").and_then(|v| v.as_u64());
    let Some(original) = original else {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "tamper",
            "ASSERTION_FAILED",
            format!("footer has no numeric events_count; footer:\n{footer}"),
            "Footer field is `events_count` (seal_v1.rs:255). If renamed, update this test.",
            &related,
        );
    };
    footer["events_count"] = serde_json::json!(original.saturating_sub(1));
    let mut new_content = String::new();
    for line in &lines[..lines.len() - 1] {
        new_content.push_str(line);
        new_content.push('\n');
    }
    new_content.push_str(&serde_json::to_string(&footer).unwrap_or_default());
    if let Err(e) = std::fs::write(&path, new_content) {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "tamper",
            "RUNTIME_ERROR",
            format!("write back: {e}"),
            "FS perms.",
            &related,
        );
    }
    stages.push("tamper_events_count".into());

    // Re-verify — must fail. Signature covers events_count so the error
    // is "footer signature verification failed" (NOT a schema error).
    if let Err(e) = session
        .run(&verify_cmd, "verification failed", Duration::from_secs(10))
        .await
        && session
            .run(&verify_cmd, "INVALID", Duration::from_secs(5))
            .await
            .is_err()
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "verify_fails_after_value_tamper",
            &e,
            "If neither 'verification failed' nor 'INVALID': the verifier accepted a tampered \
             events_count. This would be a real crypto gap. Check verify_signature \
             (orkia-audit/src/seal_v1.rs:2398) — `signature_message` must include \
             footer.events_count (it does at :2425), so a decremented count must break the sig.",
            &related,
            session,
        );
    }
    stages.push("verify_fails_after_value_tamper".into());

    // The failure must be signature-related, NOT a schema 'unknown field'
    // error (F204 covers schema; F206 is the crypto path).
    if let Err(e) = session.output().not_contains("unknown field") {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "error_is_signature_not_schema",
            &e,
            "If 'unknown field' present: schema validation fired instead of signature check. \
             But decrementing a numeric value doesn't change any field name — schema should pass, \
             signature should fail. Check validation ordering in verify_seal_v1_file.",
            &related,
            session,
        );
    }
    stages.push("error_is_signature_not_schema".into());

    pass_report(id, name, t0, stages)
}

/// F301 — agent-to-agent pipeline `@a | @b` is refused when no kernel is
/// reachable (the KernelPipelineProxy stays unattached: fail-closed gating
/// in pipeline_wiring::build), with no agent spawn and a clean upgrade
/// message. This flow runs without a kernel daemon, so the coordinator
/// slot is empty and dispatch_pipeline returns the Team-required message.
pub(crate) async fn flow_f301(session: &mut OrkiaSession) -> FlowReport {
    let id = "F301-pipeline-oss-refuse";
    let name = "Agent-to-agent pipeline refused cleanly in OSS, no spawn";
    let t0 = Instant::now();
    let mut stages = Vec::<String>::new();
    let related: Vec<String> = ["shell", "team-shell"]
        .iter()
        .map(|s| s.to_string())
        .collect();

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

    // faye + sage are pre-seeded at boot (keepalive default). The pipeline
    // must NOT spawn them — that's the whole point.
    // Type the pipeline; expect the refusal message.
    if let Err(e) = session
        .run(
            "@faye summarize | @sage review",
            "requires Orkia Team",
            Duration::from_secs(5),
        )
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "assert_refusal_message",
            &e,
            "If 'requires Orkia Team' missing: dispatch_pipeline error message changed (repl.rs:1229). \
             If output shows 'spawned': the KernelPipelineProxy got attached without a kernel — \
             pipeline_wiring::build() must return None unless the capability is present AND \
             orkia_kernel_client::discover() finds a reachable daemon (fail-closed gating). \
             In this flow no kernel daemon runs, so build_repl must hit the None branch. \
             If shell crashed: dispatch_pipeline panicked instead of returning Outcome::Error.",
            &related,
            session,
        );
    }
    stages.push("assert_refusal_message".into());

    // The upgrade URL must be present (UX requirement).
    if let Err(e) = session.output().contains("orkia.dev/team") {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "assert_upgrade_link",
            &e,
            "Refusal message must include the upgrade URL 'orkia.dev/team' (repl.rs:1231).",
            &related,
            session,
        );
    }
    stages.push("assert_upgrade_link".into());

    // Critical: no agent spawned. ps must show none running. Force reap
    // first in case a stray spawn happened and is mid-exit.
    if let Err(e) = session.force_reap().await {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "assert_no_agent_spawned",
            &e,
            "force_reap (jobs builtin) failed.",
            &related,
            session,
        );
    }
    if let Err(e) = session
        .run("ps", "none running", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "assert_no_agent_spawned",
            &e,
            "If faye or sage appear in ps: dispatch_pipeline started spawning before the \
             coordinator None-check. The None-check must be the FIRST thing in dispatch_pipeline \
             (repl.rs:1229) — before any agent dispatch side-effect.",
            &related,
            session,
        );
    }
    stages.push("assert_no_agent_spawned".into());

    // Shell must still be responsive after the refusal.
    if let Err(e) = session
        .run("whoami", Plan::Free.fixture_email(), Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "shell_still_responsive",
            &e,
            "If whoami fails after the refusal: dispatch_pipeline's error path corrupted REPL state \
             or deadlocked. Check it returns Outcome::Error cleanly to the prompt loop.",
            &related,
            session,
        );
    }
    stages.push("shell_still_responsive".into());

    pass_report(id, name, t0, stages)
}

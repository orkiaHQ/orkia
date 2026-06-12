// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! F504 — Forge App Provenance chain: tamper detection.

use super::super::shared::*;
use super::{S5_APP_RELATED, test_manifest};
use crate::report::FlowReport;
use orkia_e2e_harness::OrkiaSession;
use std::time::{Duration, Instant};

/// F504 — Forge App Provenance (ledger #3): seed a valid signed chain,
/// verify passes, flip a content byte, verify fails. The apps-side
/// counterpart to F204/F206 for RFC SEAL.
pub(crate) async fn flow_f504(session: &mut OrkiaSession) -> FlowReport {
    let id = "F504-app-seal-verify-tamper";
    let name = "Forge app provenance chain (ledger #3): tamper detection";
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

    if let Err(e) = session.seed_forge_app("sealed-app", test_manifest()) {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "seed",
            &e,
            "seed_forge_app failed.",
            &related,
            session,
        );
    }
    let chain_path = match session.seed_forge_seal_chain("sealed-app") {
        Ok(p) => p,
        Err(e) => {
            return fail_with_diagnostics(
                id,
                name,
                t0,
                &stages,
                "seed",
                &e,
                "seed_forge_seal_chain failed — SealWriter::open/append should be standalone \
                 (auto-generates the per-app key at seal/signing.pem).",
                &related,
                session,
            );
        }
    };
    stages.push("seed".into());

    if let Err(e) = session
        .run(
            "app seal sealed-app --verify",
            "events verified",
            Duration::from_secs(5),
        )
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "verify_passes_initially",
            &e,
            "If verify fails before tampering: the seeded chain is invalid. Check SealWriter signed \
             correctly. If 'verify failed' shown: the freshly-written chain didn't validate.",
            &related,
            session,
        );
    }
    stages.push("verify_passes_initially".into());

    // Tamper a known content byte ("alpha" → "alphX"): same-length value
    // mutation that breaks the recomputed SHA-256 hash (cf. F206).
    let content = match std::fs::read(&chain_path) {
        Ok(c) => c,
        Err(e) => {
            return fail_with(
                id,
                name,
                t0,
                &stages,
                "tamper",
                "RUNTIME_ERROR",
                format!("read chain: {e}"),
                "FS perms.",
                &related,
            );
        }
    };
    let needle = b"alpha";
    let Some(pos) = content.windows(needle.len()).position(|w| w == needle) else {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "tamper",
            "ASSERTION_FAILED",
            "seeded marker 'alpha' not found in chain".into(),
            "seed_forge_seal_chain should append data {\"marker\":\"alpha\"}; if absent the seed changed.",
            &related,
        );
    };
    let mut tampered = content;
    tampered[pos + 4] = b'X'; // "alpha" -> "alphX"
    if let Err(e) = std::fs::write(&chain_path, tampered) {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "tamper",
            "RUNTIME_ERROR",
            format!("write chain: {e}"),
            "FS perms.",
            &related,
        );
    }
    stages.push("tamper".into());

    if let Err(e) = session
        .run(
            "app seal sealed-app --verify",
            "verify failed",
            Duration::from_secs(5),
        )
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "verify_fails_after_tamper",
            &e,
            "If verify still passes after tampering: verify_chain doesn't detect the mutation. \
             It must recompute the SHA-256 hash chain and/or check the ECDSA signature \
             (orkia-forge-seal verifier.rs). This is the ledger-#3 equivalent of F204/F206.",
            &related,
            session,
        );
    }
    stages.push("verify_fails_after_tamper".into());

    pass_report(id, name, t0, stages)
}

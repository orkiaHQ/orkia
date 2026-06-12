// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! SEAL — append-only hash-linked audit chains, scoped per job and
//! per project.
//!
//! Old layout (deleted): one global `~/.orkia/seal.jsonl` chain
//! held by a single `SealEmitter`. Replaced because the global
//! mixed every event (decision/outcome/hook/agent.spawn/RFC/…)
//! across every agent and job — unreadable past a few hundred
//! records, and verification re-walks the entire history for any
//! tamper check.
//!
//! New layout (this module):
//! * One chain per agent job at
//!   `<data_dir>/agents/<agent>/jobs/<job_id>/seal.jsonl`. Closed
//!   on `agent.complete` / `agent.failed`; the closing tip hash is
//!   embedded into the project chain as a `job.reference` record.
//! * One chain per project at
//!   `<data_dir>/projects/<project>/seal.jsonl`. Open indefinitely.

pub use orkia_shell_types::seal::*;

pub mod audit;
pub mod audit_verify;
pub mod chain;
pub mod consumer;
pub mod emit;
pub mod manager;
pub mod migrate;
pub mod pending;

pub use audit::render as render_audit;
pub use chain::{SealChain, ZERO_HASH, compute_hash};
pub use consumer::{JobProjects, route as route_event, spawn as spawn_consumer};
pub use emit::{ScopeChangeEvent, emit_scope_event};
pub use manager::{DeepVerifyResult, JobVerifyResult, SealManager};
pub use migrate::migrate_global_seal;
pub use pending::ScheduledContext;

use sha2::{Digest, Sha256};
use std::path::Path;

/// Compact SHA-256 (first 16 hex chars) of a file's bytes, for SEAL
/// detail payloads. Used by the RFC builtins to embed proof-of-
/// content into `rfc.create` / `rfc.update` / `rfc.complete` records.
pub fn rfc_content_hash(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            let full = hex::encode(hasher.finalize());
            full.chars().take(16).collect()
        }
        Err(_) => String::new(),
    }
}

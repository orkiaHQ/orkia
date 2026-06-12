// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use orkia_rfc_core::RfcId;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SealRecord {
    pub seq: u64,
    pub timestamp: String,
    pub event_type: String,
    pub detail: serde_json::Value,
    pub hash: String,
    pub prev_hash: String,

    /// Identifies the RFC this event was emitted in the context of.
    ///
    /// `None` means the event happened outside any RFC scope. Events with
    /// `Some(rfc_id)` are picked up by the `SealV1Assembler` at RFC closure
    ///
    /// The hash chain treats `None` as **absent from the hashed bytes** so
    /// chains written before this field existed remain verifiable with the
    /// new code (see `chain::compute_hash`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rfc_id: Option<RfcId>,
}

/// Failure mode for a SEAL chain append.
///
/// `SealChain::append` is fail-closed: persistence failures surface as
/// `Err` so callers can refuse to emit any downstream signal (journal
/// event, network send) that would imply the record is durable when it
#[derive(Debug, thiserror::Error)]
pub enum SealError {
    /// Disk write failed (full disk, permission denied, etc.).
    #[error("seal chain append failed: I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The chain is closed (terminal event already appended) and rejects
    /// further appends.
    #[error("seal chain append failed: chain is closed")]
    Closed,

    /// Record serialization failed before any disk write was attempted.
    #[error("seal chain append failed: serialization error: {0}")]
    Serialize(#[from] serde_json::Error),

    /// A broken internal invariant (should be unreachable). Returned instead
    /// of panicking in a 24/7 shell (BUG-080).
    #[error("seal chain internal error: {0}")]
    Internal(String),
}

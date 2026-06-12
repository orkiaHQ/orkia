// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! The on-disk record shape.
//!
//! Field order matters because we hash a serde_json::to_vec of the
//! record-minus-hash-and-sig. As long as the same crate writes and reads,
//! field order is consistent — serde preserves struct order.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Genesis `prev_hash`. Every other record's `prev_hash` is the
/// previous record's `hash`.
pub const GENESIS_PREV: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SealRecord {
    /// Monotonically increasing per-app. First record = 1.
    pub id: u64,
    /// RFC3339 timestamp at append time.
    pub ts: DateTime<Utc>,
    /// `sha256:<hex>` of the previous record (or GENESIS_PREV for #1).
    pub prev_hash: String,
    /// Event kind — `app.window.opened`, `app.network.fetch`, etc.
    pub kind: String,
    /// Kind-specific payload. Free-form JSON.
    pub data: serde_json::Value,
    /// `sha256:<hex>` of the canonical JSON of this record with `hash`
    /// + `sig` removed. Populated by the writer.
    pub hash: String,
    /// Hex-encoded DER ECDSA signature over `hash`. Populated by the writer.
    pub sig: String,
}

/// Same shape as [`SealRecord`] but without the populated `hash`/`sig`.
/// Used internally to build a record, then compute hash + sign, then
/// fold those in.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct UnsignedRecord<'a> {
    pub id: u64,
    pub ts: DateTime<Utc>,
    pub prev_hash: &'a str,
    pub kind: &'a str,
    pub data: &'a serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_serde_round_trip() {
        let r = SealRecord {
            id: 1,
            ts: Utc::now(),
            prev_hash: GENESIS_PREV.into(),
            kind: "app.window.opened".into(),
            data: serde_json::json!({"window": "main"}),
            hash: "sha256:abc".into(),
            sig: "deadbeef".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        let parsed: SealRecord = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed.kind, "app.window.opened");
        assert_eq!(parsed.prev_hash, GENESIS_PREV);
    }
}

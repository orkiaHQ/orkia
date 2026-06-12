// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Walk a per-app SEAL chain and verify every record.
//!
//! Two checks per record:
//! 1. `hash` matches SHA-256 of the canonical JSON of `{id, ts, prev_hash, kind, data}`.
//! 2. `sig` is a valid ECDSA P-256 DER signature over `hash` by the
//!    per-app verifying key (loaded from `signing.pem`).
//!
//! Plus chain invariants:
//! 3. `id` is strictly monotonic from 1 in the file order.
//! 4. `prev_hash` of record N equals `hash` of record N-1
//!    (or `GENESIS_PREV` when N == 1).

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use p256::ecdsa::Signature;
use p256::ecdsa::signature::Verifier;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::key::{SealKey, SealKeyError};
use crate::record::{GENESIS_PREV, SealRecord, UnsignedRecord};

#[derive(Debug, Error)]
pub enum VerifyError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("key: {0}")]
    Key(#[from] SealKeyError),
    #[error("parse record #{id}: {msg}")]
    Parse { id: u64, msg: String },
    #[error("hash mismatch on record #{id}")]
    HashMismatch { id: u64 },
    #[error("chain broken at record #{id}: prev_hash != previous record's hash")]
    ChainBroken { id: u64 },
    #[error("id sequence broken: expected #{expected}, got #{found}")]
    IdSkip { expected: u64, found: u64 },
    #[error("signature invalid on record #{id}")]
    BadSignature { id: u64 },
    #[error("bad signature encoding on record #{id}: {msg}")]
    SigDecode { id: u64, msg: String },
}

#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub events: u64,
    pub last_hash: String,
}

/// Verify `<seal-dir>/events.jsonl` against the key at
/// `<seal-dir>/signing.pem`.
pub fn verify_chain(seal_dir: &Path) -> Result<VerifyReport, VerifyError> {
    let key_path = seal_dir.join("signing.pem");
    let events_path = seal_dir.join("events.jsonl");
    let key = SealKey::load(&key_path)?;
    let verifying = *key.verifying_key();

    let f = File::open(&events_path)?;
    let reader = BufReader::new(f);
    let mut prev_hash = GENESIS_PREV.to_string();
    let mut expected_id: u64 = 1;
    let mut count: u64 = 0;
    let mut last_hash = GENESIS_PREV.to_string();

    for (line_no, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let rec: SealRecord = serde_json::from_str(&line).map_err(|e| VerifyError::Parse {
            id: expected_id,
            msg: format!("line {}: {e}", line_no + 1),
        })?;

        // 3. id monotonic.
        if rec.id != expected_id {
            return Err(VerifyError::IdSkip {
                expected: expected_id,
                found: rec.id,
            });
        }
        // 4. chain link.
        if rec.prev_hash != prev_hash {
            return Err(VerifyError::ChainBroken { id: rec.id });
        }
        // 1. hash matches content.
        let unsigned = UnsignedRecord {
            id: rec.id,
            ts: rec.ts,
            prev_hash: &rec.prev_hash,
            kind: &rec.kind,
            data: &rec.data,
        };
        let unsigned_bytes = serde_json::to_vec(&unsigned).map_err(|e| VerifyError::Parse {
            id: rec.id,
            msg: e.to_string(),
        })?;
        let recomputed = format!("sha256:{}", sha256_hex(&unsigned_bytes));
        if recomputed != rec.hash {
            return Err(VerifyError::HashMismatch { id: rec.id });
        }
        // 2. signature over hash with the per-app verifying key.
        let sig_bytes = hex::decode(&rec.sig).map_err(|e| VerifyError::SigDecode {
            id: rec.id,
            msg: e.to_string(),
        })?;
        let sig = Signature::from_der(&sig_bytes).map_err(|e| VerifyError::SigDecode {
            id: rec.id,
            msg: e.to_string(),
        })?;
        verifying
            .verify(rec.hash.as_bytes(), &sig)
            .map_err(|_| VerifyError::BadSignature { id: rec.id })?;

        prev_hash = rec.hash.clone();
        last_hash = rec.hash;
        expected_id += 1;
        count += 1;
    }

    Ok(VerifyReport {
        events: count,
        last_hash,
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::SealWriter;
    use tempfile::TempDir;

    #[test]
    fn verifies_empty_chain() {
        let tmp = TempDir::new().unwrap();
        let _w = SealWriter::open(tmp.path()).unwrap();
        let report = verify_chain(tmp.path()).unwrap();
        assert_eq!(report.events, 0);
        assert_eq!(report.last_hash, GENESIS_PREV);
    }

    #[test]
    fn verifies_one_record() {
        let tmp = TempDir::new().unwrap();
        let w = SealWriter::open(tmp.path()).unwrap();
        w.append("app.window.opened", serde_json::json!({}))
            .unwrap();
        let report = verify_chain(tmp.path()).unwrap();
        assert_eq!(report.events, 1);
    }

    #[test]
    fn verifies_long_chain() {
        let tmp = TempDir::new().unwrap();
        let w = SealWriter::open(tmp.path()).unwrap();
        for i in 0..25 {
            w.append("kind", serde_json::json!({"i": i})).unwrap();
        }
        let report = verify_chain(tmp.path()).unwrap();
        assert_eq!(report.events, 25);
    }
}

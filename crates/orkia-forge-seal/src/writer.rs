// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Append-only hash-chained writer for `<app-dir>/seal/events.jsonl`.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::Utc;
use p256::ecdsa::Signature;
use p256::ecdsa::signature::Signer;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::key::{SealKey, SealKeyError};
use crate::record::{GENESIS_PREV, SealRecord, UnsignedRecord};

#[derive(Debug, Error)]
pub enum SealWriterError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("key: {0}")]
    Key(#[from] SealKeyError),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("corrupt previous record: {0}")]
    Corrupt(String),
}

/// Single-writer SEAL appender for one app.
///
/// Multiple Tauri command handlers may call `append` concurrently (the
/// Tauri runtime parallelizes command futures on a multi-threaded
/// executor). The internal `Mutex` serializes:
///   1. reading the last record (to compute `prev_hash` + `next id`)
///   2. building + signing the new record
///   3. appending the JSONL line.
///
/// All three must happen atomically or two concurrent appends could
/// agree on the same `prev_hash` and produce a fork in the chain.
pub struct SealWriter {
    inner: Mutex<Inner>,
}

struct Inner {
    path: PathBuf,
    key: SealKey,
}

impl SealWriter {
    /// Open or create the writer for `<app-dir>/seal/`.
    pub fn open(seal_dir: &Path) -> Result<Self, SealWriterError> {
        std::fs::create_dir_all(seal_dir)?;
        let key = SealKey::load_or_generate(seal_dir)?;
        let path = seal_dir.join("events.jsonl");
        // Touch the file so verifier code can rely on existence.
        if !path.exists() {
            std::fs::File::create(&path)?;
        }
        Ok(Self {
            inner: Mutex::new(Inner { path, key }),
        })
    }

    /// Append a new event. Returns the new event's id.
    ///
    /// **Chain integrity note (SEC-079):** this method reads the tail record
    /// to obtain `(prev_id, prev_hash)` and chains the new record to it.
    /// It does **not** verify that the pre-existing chain is intact — that
    /// is the responsibility of [`crate::verify::verify_chain`], which should
    /// be called separately when integrity must be confirmed. An append to a
    /// chain whose tail has been tampered with will produce a structurally
    /// valid new record that nonetheless extends a corrupted chain;
    /// `verify_chain` will detect the corruption when invoked.
    pub fn append(&self, kind: &str, data: serde_json::Value) -> Result<u64, SealWriterError> {
        let guard = self.inner.lock();
        let (next_id, prev_hash) = read_tail(&guard.path)?;
        let unsigned = UnsignedRecord {
            id: next_id,
            ts: Utc::now(),
            prev_hash: &prev_hash,
            kind,
            data: &data,
        };
        let unsigned_bytes = serde_json::to_vec(&unsigned)?;
        let hash = sha256_hex(&unsigned_bytes);
        let hash_str = format!("sha256:{hash}");
        let sig: Signature = guard.key.signing_key().sign(hash_str.as_bytes());
        let sig_hex = hex::encode(sig.to_der().as_bytes());
        let full = SealRecord {
            id: next_id,
            ts: unsigned.ts,
            prev_hash: prev_hash.clone(),
            kind: kind.to_string(),
            data,
            hash: hash_str,
            sig: sig_hex,
        };
        let mut line = serde_json::to_vec(&full)?;
        line.push(b'\n');
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&guard.path)?;
        f.write_all(&line)?;
        // Encourage the kernel to flush so a process kill mid-batch
        // doesn't lose the last record's tail bytes.
        f.flush()?;
        Ok(next_id)
    }
}

/// Returns `(next_id, prev_hash)` for the new record.
fn read_tail(path: &Path) -> Result<(u64, String), SealWriterError> {
    if !path.exists() {
        return Ok((1, GENESIS_PREV.into()));
    }
    let f = std::fs::File::open(path)?;
    let reader = BufReader::new(f);
    let mut last_line: Option<String> = None;
    for line in reader.lines() {
        let line = line?;
        if !line.trim().is_empty() {
            last_line = Some(line);
        }
    }
    let Some(line) = last_line else {
        return Ok((1, GENESIS_PREV.into()));
    };
    let parsed: SealRecord = serde_json::from_str(&line)
        .map_err(|e| SealWriterError::Corrupt(format!("tail record: {e}")))?;
    Ok((parsed.id + 1, parsed.hash))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn first_append_uses_genesis_prev() {
        let tmp = TempDir::new().unwrap();
        let w = SealWriter::open(tmp.path()).unwrap();
        let id = w
            .append("app.window.opened", serde_json::json!({"window": "main"}))
            .unwrap();
        assert_eq!(id, 1);
        let line = std::fs::read_to_string(tmp.path().join("events.jsonl")).unwrap();
        let parsed: SealRecord = serde_json::from_str(line.lines().next().unwrap()).unwrap();
        assert_eq!(parsed.id, 1);
        assert_eq!(parsed.prev_hash, GENESIS_PREV);
        assert!(parsed.hash.starts_with("sha256:"));
        assert!(!parsed.sig.is_empty());
    }

    #[test]
    fn chain_links_correctly() {
        let tmp = TempDir::new().unwrap();
        let w = SealWriter::open(tmp.path()).unwrap();
        w.append("a", serde_json::json!({"x": 1})).unwrap();
        w.append("b", serde_json::json!({"x": 2})).unwrap();
        let body = std::fs::read_to_string(tmp.path().join("events.jsonl")).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        let r1: SealRecord = serde_json::from_str(lines[0]).unwrap();
        let r2: SealRecord = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(r2.prev_hash, r1.hash, "chain link");
        assert_eq!(r2.id, r1.id + 1, "id monotonic");
        assert_ne!(r1.hash, r2.hash, "different content → different hash");
    }

    #[test]
    fn reopening_continues_chain() {
        let tmp = TempDir::new().unwrap();
        {
            let w = SealWriter::open(tmp.path()).unwrap();
            w.append("a", serde_json::json!({})).unwrap();
        }
        {
            let w = SealWriter::open(tmp.path()).unwrap();
            let id = w.append("b", serde_json::json!({})).unwrap();
            assert_eq!(id, 2);
        }
        let body = std::fs::read_to_string(tmp.path().join("events.jsonl")).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn concurrent_appends_serialize() {
        use std::sync::Arc;
        use std::thread;
        let tmp = TempDir::new().unwrap();
        let w = Arc::new(SealWriter::open(tmp.path()).unwrap());
        let mut threads = Vec::new();
        for i in 0..8 {
            let w = w.clone();
            threads.push(thread::spawn(move || {
                w.append("concurrent", serde_json::json!({"i": i})).unwrap()
            }));
        }
        for t in threads {
            t.join().unwrap();
        }
        let body = std::fs::read_to_string(tmp.path().join("events.jsonl")).unwrap();
        let mut ids: Vec<u64> = body
            .lines()
            .map(|l| serde_json::from_str::<SealRecord>(l).unwrap().id)
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![1, 2, 3, 4, 5, 6, 7, 8],
            "all ids assigned uniquely"
        );
    }
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Per-chain SEAL cursor.
//!
//! Cursor file format (`~/.orkia/state/stream/seal.cursor`) is JSONL —
//! one line per chain:
//!
//! ```jsonl
//! {"chain_id":"my-project","offset":12345,"last_hash":"abc..."}
//! ```
//!
//! Atomic writes: temp file + rename.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::batch::Batch;

const SEAL_CURSOR_FILE: &str = "seal.cursor";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainCursor {
    pub chain_id: String,
    pub offset: u64,
    pub last_hash: String,
}

#[derive(Debug, Default, Clone)]
pub struct SealCursor {
    chains: HashMap<String, ChainCursor>,
}

impl SealCursor {
    pub fn load_or_default(state_dir: &Path) -> Self {
        let path = state_dir.join(SEAL_CURSOR_FILE);
        let mut cursor = Self::default();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => return cursor,
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<ChainCursor>(line) {
                Ok(c) => {
                    cursor.chains.insert(c.chain_id.clone(), c);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "orkia-stream: skipping malformed cursor line");
                }
            }
        }
        cursor
    }

    pub fn get(&self, chain_id: &str) -> Option<&ChainCursor> {
        self.chains.get(chain_id)
    }

    pub fn set(&mut self, chain_id: &str, offset: u64, last_hash: String) {
        self.chains.insert(
            chain_id.to_string(),
            ChainCursor {
                chain_id: chain_id.to_string(),
                offset,
                last_hash,
            },
        );
    }

    /// Apply a batch's seal entries to the cursor map.
    pub fn advance(&mut self, batch: &Batch) {
        for adv in batch.seal_advances() {
            self.set(&adv.chain_id, adv.byte_end, adv.last_hash.clone());
        }
    }

    pub fn persist(&self, state_dir: &Path) {
        if let Err(e) = self.persist_inner(state_dir) {
            tracing::error!(error = %e, "orkia-stream: failed to persist seal cursor");
        }
    }

    fn persist_inner(&self, state_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(state_dir)?;
        let final_path = state_dir.join(SEAL_CURSOR_FILE);
        let tmp_path = state_dir.join(format!("{SEAL_CURSOR_FILE}.tmp"));
        {
            let mut f = std::fs::File::create(&tmp_path)?;
            for c in self.chains.values() {
                let line = serde_json::to_string(c).map_err(std::io::Error::other)?;
                writeln!(f, "{line}")?;
            }
            f.flush()?;
            f.sync_all()?;
        }
        std::fs::rename(tmp_path, final_path)?;
        Ok(())
    }

    pub fn chain_count(&self) -> usize {
        self.chains.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn build_batch(advances: Vec<(&str, u64, &str)>) -> Batch {
        let mut b = Batch::default();
        for (chain, off, hash) in advances {
            b.add_seal_advance(chain.to_string(), 0, hash.to_string(), off, None);
        }
        b
    }

    #[test]
    fn roundtrip_atomic() {
        let dir = tempdir().unwrap();
        let mut c = SealCursor::default();
        c.set("workspace", 42, "abc".into());
        c.set("proj/x", 9, "def".into());
        c.persist(dir.path());

        let loaded = SealCursor::load_or_default(dir.path());
        assert_eq!(loaded.chain_count(), 2);
        assert_eq!(loaded.get("workspace").unwrap().offset, 42);
        assert_eq!(loaded.get("proj/x").unwrap().last_hash, "def");
    }

    #[test]
    fn advance_from_batch() {
        let mut c = SealCursor::default();
        c.advance(&build_batch(vec![("workspace", 100, "h1")]));
        assert_eq!(c.get("workspace").unwrap().offset, 100);
    }

    #[test]
    fn missing_cursor_file_loads_default() {
        let dir = tempdir().unwrap();
        let c = SealCursor::load_or_default(dir.path());
        assert_eq!(c.chain_count(), 0);
    }

    #[test]
    fn malformed_lines_skipped_on_load() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            dir.path().join(SEAL_CURSOR_FILE),
            "not-json\n{\"chain_id\":\"ok\",\"offset\":1,\"last_hash\":\"h\"}\n",
        )
        .unwrap();
        let c = SealCursor::load_or_default(dir.path());
        assert_eq!(c.chain_count(), 1);
    }
}

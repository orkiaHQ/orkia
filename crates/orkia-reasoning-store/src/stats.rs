// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! Read-only roll-ups for the `$reasoning status` builtin. Cheap `COUNT(*)`
//! queries over a fresh (reader) connection — never on the hot path.

use crate::{ReasoningStore, StoreError};

/// A snapshot of what the local store currently holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreStats {
    /// Reasoning sessions captured locally.
    pub sessions: u64,
    /// Turns captured across all sessions.
    pub turns: u64,
    /// Active (non-superseded) knowledge nodes pulled from the cloud.
    pub nodes: u64,
    /// Turns still awaiting a successful push to the cloud.
    pub dirty_turns: u64,
    /// Preference signals still awaiting a successful push.
    pub dirty_signals: u64,
}

impl ReasoningStore {
    /// Roll up local row counts for the status surface.
    pub fn stats(&self) -> Result<StoreStats, StoreError> {
        Ok(StoreStats {
            sessions: self.count("SELECT COUNT(*) FROM session")?,
            turns: self.count("SELECT COUNT(*) FROM turn")?,
            nodes: self.count("SELECT COUNT(*) FROM knowledge_node WHERE superseded_at IS NULL")?,
            dirty_turns: self.count("SELECT COUNT(*) FROM turn WHERE dirty = 1")?,
            dirty_signals: self.count("SELECT COUNT(*) FROM preference_signal WHERE dirty = 1")?,
        })
    }

    fn count(&self, sql: &str) -> Result<u64, StoreError> {
        let n: i64 = self.conn.query_row(sql, [], |r| r.get(0))?;
        Ok(n.max(0) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_store_has_zero_stats() {
        let s = ReasoningStore::in_memory().unwrap();
        assert_eq!(s.stats().unwrap(), StoreStats::default());
    }
}

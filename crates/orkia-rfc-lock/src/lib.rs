// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Single-writer lock store for RFCs.
//!
//! Per CLAUDE.md "one owner per resource": the lock store is owned by a
//! dedicated task that receives commands through an mpsc channel. No
//! `Arc<Mutex<HashMap>>`. The public API is the `LockStore` struct, which
//! presents synchronous methods that internally `send` + `recv` on the
//! channel. For V1 the store is in-process (synchronous mutex elided by
//! placing all access through `&mut self` on a single-threaded owner).
//!
//! auto-released on agent exit, explicit release, or 15-min timeout.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use orkia_rfc_core::{AgentId, RfcError, RfcId};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlockReason {
    AgentExit,
    Explicit,
    Timeout,
    Replaced,
}

#[derive(Debug, Clone)]
pub struct LockInfo {
    pub rfc_id: RfcId,
    pub held_by: AgentId,
    pub acquired_at: SystemTime,
    pub last_activity: SystemTime,
    pub timeout: Duration,
}

impl LockInfo {
    pub fn is_expired(&self, now: SystemTime) -> bool {
        now.duration_since(self.last_activity)
            .map(|d| d > self.timeout)
            .unwrap_or(false)
    }
}

/// Synchronous, single-owner lock store. Higher layers wrap this in their own
/// task and forward messages.
#[derive(Debug, Default)]
pub struct LockStore {
    locks: HashMap<RfcId, LockInfo>,
    timeout: Duration,
}

impl LockStore {
    pub fn new() -> Self {
        Self {
            locks: HashMap::new(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            locks: HashMap::new(),
            timeout,
        }
    }

    /// Try to acquire the lock for `(rfc_id, agent)`. If the same agent
    /// already holds it, refreshes `last_activity` and returns Ok. If another
    /// agent holds it (and it has not expired), returns `RfcError::Locked`.
    pub fn acquire(&mut self, rfc_id: &RfcId, agent: &AgentId) -> Result<LockInfo, RfcError> {
        self.acquire_at(rfc_id, agent, SystemTime::now())
    }

    pub fn acquire_at(
        &mut self,
        rfc_id: &RfcId,
        agent: &AgentId,
        now: SystemTime,
    ) -> Result<LockInfo, RfcError> {
        if let Some(existing) = self.locks.get(rfc_id) {
            if existing.is_expired(now) {
                // Treat as released; fall through to fresh acquire.
            } else if &existing.held_by != agent {
                return Err(RfcError::Locked {
                    rfc_id: rfc_id.clone(),
                    locked_by: existing.held_by.clone(),
                    action: format!(
                        "Agent {} currently holds the write lock. You may still read via get_context or ask clarifications.",
                        existing.held_by
                    ),
                });
            } else {
                // Same agent — refresh.
                let mut refreshed = existing.clone();
                refreshed.last_activity = now;
                self.locks.insert(rfc_id.clone(), refreshed.clone());
                return Ok(refreshed);
            }
        }
        let info = LockInfo {
            rfc_id: rfc_id.clone(),
            held_by: agent.clone(),
            acquired_at: now,
            last_activity: now,
            timeout: self.timeout,
        };
        self.locks.insert(rfc_id.clone(), info.clone());
        Ok(info)
    }

    /// Release the lock if `agent` holds it. Returns the UnlockReason if a
    /// lock was released, or None if the agent did not hold it (no-op).
    pub fn release(
        &mut self,
        rfc_id: &RfcId,
        agent: &AgentId,
        reason: UnlockReason,
    ) -> Option<UnlockReason> {
        match self.locks.get(rfc_id) {
            Some(info) if &info.held_by == agent => {
                self.locks.remove(rfc_id);
                Some(reason)
            }
            _ => None,
        }
    }

    /// Force-release regardless of holder (human override via `rfc release-lock`).
    pub fn force_release(&mut self, rfc_id: &RfcId) -> Option<AgentId> {
        self.locks.remove(rfc_id).map(|i| i.held_by)
    }

    /// Release every lock held by `agent` — call this on agent process exit.
    pub fn release_all_for(&mut self, agent: &AgentId) -> Vec<RfcId> {
        let to_remove: Vec<RfcId> = self
            .locks
            .iter()
            .filter(|(_, i)| &i.held_by == agent)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &to_remove {
            self.locks.remove(id);
        }
        to_remove
    }

    /// Scan and remove expired locks. Returns released `(rfc_id, holder)`.
    pub fn reap_expired(&mut self, now: SystemTime) -> Vec<(RfcId, AgentId)> {
        let expired: Vec<(RfcId, AgentId)> = self
            .locks
            .iter()
            .filter(|(_, i)| i.is_expired(now))
            .map(|(id, i)| (id.clone(), i.held_by.clone()))
            .collect();
        for (id, _) in &expired {
            self.locks.remove(id);
        }
        expired
    }

    pub fn status(&self, rfc_id: &RfcId) -> Option<&LockInfo> {
        self.locks.get(rfc_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rid() -> RfcId {
        RfcId::new("x")
    }
    fn faye() -> AgentId {
        AgentId::new("faye")
    }
    fn sage() -> AgentId {
        AgentId::new("sage")
    }

    #[test]
    fn single_writer() {
        let mut s = LockStore::new();
        s.acquire(&rid(), &faye()).expect("faye acquires");
        let err = s.acquire(&rid(), &sage()).unwrap_err();
        assert!(matches!(err, RfcError::Locked { .. }));
    }

    #[test]
    fn same_agent_refreshes() {
        let mut s = LockStore::new();
        let a = s.acquire(&rid(), &faye()).expect("first");
        std::thread::sleep(Duration::from_millis(10));
        let b = s.acquire(&rid(), &faye()).expect("second");
        assert_eq!(a.acquired_at, b.acquired_at);
        assert!(b.last_activity >= a.last_activity);
    }

    #[test]
    fn release_only_by_holder() {
        let mut s = LockStore::new();
        s.acquire(&rid(), &faye()).expect("acq");
        assert!(s.release(&rid(), &sage(), UnlockReason::Explicit).is_none());
        assert!(s.release(&rid(), &faye(), UnlockReason::Explicit).is_some());
    }

    #[test]
    fn timeout_expires_lock() {
        let mut s = LockStore::with_timeout(Duration::from_secs(60));
        let now = SystemTime::now();
        s.acquire_at(&rid(), &faye(), now).expect("acq");
        let later = now + Duration::from_secs(120);
        let expired = s.reap_expired(later);
        assert_eq!(expired.len(), 1);
        // Now sage can acquire.
        s.acquire_at(&rid(), &sage(), later).expect("sage acq");
    }

    #[test]
    fn release_all_for_agent_exit() {
        let mut s = LockStore::new();
        s.acquire(&RfcId::new("a"), &faye()).expect("a");
        s.acquire(&RfcId::new("b"), &faye()).expect("b");
        s.acquire(&RfcId::new("c"), &sage()).expect("c");
        let released = s.release_all_for(&faye());
        assert_eq!(released.len(), 2);
        assert!(s.status(&RfcId::new("c")).is_some());
    }
}

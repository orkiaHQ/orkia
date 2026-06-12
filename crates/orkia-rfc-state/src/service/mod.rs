// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! `RfcStateService` — the high-level orchestrator used by the MCP tool
//! server and the shell builtins.
//!
//! All mutating methods:
//! 1. Load the current record.
//! 2. Check the per-state allow matrix via [`tool_allowed`].
//! 3. Check optional snapshot-staleness via `if_hash_matches`.
//! 4. Acquire/refresh the write lock when needed.
//! 5. Apply the change to the filesystem (atomic via `RfcStore::save`).
//! 6. Emit one or more [`RfcEvent`]s through the sink.
//!
//! State-machine validation lives in [`orkia_rfc_core::validate_transition`].
//! Approval enforcement is the *caller's* job — the service trusts that the
//! shell has already obtained human approval for `promote`/`complete`/
//! `abandon`/`reopen`.

mod locks;
mod queries;
mod transitions;

#[cfg(test)]
#[path = "tests.rs"]
mod tests_mod;

use serde::{Deserialize, Serialize};
use std::sync::Mutex;

use orkia_rfc_core::{AgentId, ContentHash, DecisionId, RfcId, RfcState, RfcStore};
use orkia_rfc_lock::LockStore;

use crate::events::EventSink;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RfcContext {
    pub rfc_id: RfcId,
    pub state: RfcState,
    pub version: u32,
    pub content: String,
    pub content_hash: ContentHash,
    pub locked_by: Option<AgentId>,
    pub open_clarifications: u32,
    pub unreviewed_decisions: u32,
}

#[derive(Debug, Clone)]
pub struct AskRequest {
    pub rfc_id: RfcId,
    pub agent: AgentId,
    pub question: String,
    pub rationale: String,
}

#[derive(Debug, Clone)]
pub struct LogDecisionRequest {
    pub rfc_id: RfcId,
    pub agent: AgentId,
    pub content: String,
    pub rationale: String,
    pub affects: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct EditRequest {
    pub rfc_id: RfcId,
    pub agent: AgentId,
    pub section: orkia_rfc_core::SectionPath,
    /// New full body. (Diff application is a UI concern; the service operates
    /// on resolved bodies for simplicity and atomicity.)
    pub new_body: String,
    pub linked_decisions: Vec<DecisionId>,
    pub if_hash_matches: Option<ContentHash>,
}

/// Single-owner service. Wrap in a dedicated task (`mpsc` command channel)
/// to expose to multi-threaded consumers per the "one owner per resource"
/// rule. For tests and single-threaded uses, hold it behind a `Mutex`.
pub struct RfcStateService {
    pub(super) store: RfcStore,
    pub(super) locks: Mutex<LockStore>,
    pub(super) sink: Box<dyn EventSink>,
    pub(super) decision_seq: Mutex<u32>,
}

impl RfcStateService {
    pub fn new(store: RfcStore, sink: Box<dyn EventSink>) -> Self {
        // Resume the decision counter from what's already on disk so a restart
        // doesn't re-issue `d-001…` over existing decisions (BUG-034).
        let seq = initial_decision_seq(&store);
        Self {
            store,
            locks: Mutex::new(LockStore::new()),
            sink,
            decision_seq: Mutex::new(seq),
        }
    }

    /// Construct a service with a non-default lock timeout. Used by tests
    /// and by the shell's reaper task (which leaves the default 15m but
    /// might want a shorter cadence in CI environments).
    pub fn with_lock_timeout(
        store: RfcStore,
        sink: Box<dyn EventSink>,
        timeout: std::time::Duration,
    ) -> Self {
        let seq = initial_decision_seq(&store);
        Self {
            store,
            locks: Mutex::new(LockStore::with_timeout(timeout)),
            sink,
            decision_seq: Mutex::new(seq),
        }
    }

    pub fn store(&self) -> &RfcStore {
        &self.store
    }

    pub(super) fn next_decision_id(&self) -> DecisionId {
        let mut g = match self.decision_seq.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *g += 1;
        DecisionId::new(format!("d-{:03}", *g))
    }
}

/// Highest `d-NNN` decision number already persisted across all RFCs, so the
/// in-memory counter resumes past it after a restart instead of colliding with
/// existing decision IDs (BUG-034).
fn initial_decision_seq(store: &RfcStore) -> u32 {
    let Ok(rfcs) = store.list() else {
        return 0;
    };
    let mut max = 0u32;
    for rec in &rfcs {
        if let Ok(decisions) = store.read_decisions(&rec.fm.id) {
            for d in &decisions {
                if let Some(n) =
                    d.id.as_str()
                        .strip_prefix("d-")
                        .and_then(|s| s.parse::<u32>().ok())
                {
                    max = max.max(n);
                }
            }
        }
    }
    max
}

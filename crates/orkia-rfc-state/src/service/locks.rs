// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

use orkia_rfc_core::{AgentId, RfcId};
use orkia_rfc_lock::UnlockReason;

use crate::events::RfcEvent;

use super::RfcStateService;

impl RfcStateService {
    pub fn release_lock(&self, rfc_id: &RfcId, agent: &AgentId, reason: UnlockReason) -> bool {
        let released = {
            let Ok(mut locks) = self.locks.lock() else {
                return false;
            };
            locks.release(rfc_id, agent, reason)
        };
        if released.is_some() {
            self.sink.emit(RfcEvent::Unlocked {
                rfc_id: rfc_id.clone(),
                by: agent.clone(),
                reason: reason.into(),
            });
            true
        } else {
            false
        }
    }

    pub fn release_all_for(&self, agent: &AgentId) {
        let released = {
            let Ok(mut locks) = self.locks.lock() else {
                return;
            };
            locks.release_all_for(agent)
        };
        for rid in released {
            self.sink.emit(RfcEvent::Unlocked {
                rfc_id: rid,
                by: agent.clone(),
                reason: UnlockReason::AgentExit.into(),
            });
        }
    }

    /// Force-release the lock for `rfc_id`, regardless of holder. Used by
    /// the human-override `rfc release-lock` builtin. Returns the holder
    /// that was forcibly released (if any).
    pub fn force_release(&self, rfc_id: &RfcId) -> Option<AgentId> {
        let released = {
            let Ok(mut locks) = self.locks.lock() else {
                return None;
            };
            locks.force_release(rfc_id)
        };
        if let Some(by) = released.clone() {
            self.sink.emit(RfcEvent::Unlocked {
                rfc_id: rfc_id.clone(),
                by,
                reason: UnlockReason::Replaced.into(),
            });
        }
        released
    }

    /// Scan and release expired locks. Emits one `rfc.unlocked` event with
    /// `reason: Timeout` per release. Intended to be called periodically by
    /// a background reaper task (default cadence: every 60s in the shell).
    /// Returns the count of released locks for observability.
    pub fn reap_expired_locks(&self, now: std::time::SystemTime) -> usize {
        let released = {
            let Ok(mut locks) = self.locks.lock() else {
                return 0;
            };
            locks.reap_expired(now)
        };
        let count = released.len();
        for (rfc_id, holder) in released {
            self.sink.emit(RfcEvent::Unlocked {
                rfc_id,
                by: holder,
                reason: UnlockReason::Timeout.into(),
            });
        }
        count
    }
}

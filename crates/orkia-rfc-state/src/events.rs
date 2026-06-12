// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

use serde::{Deserialize, Serialize};
use std::sync::Mutex;

use orkia_rfc_core::{AgentId, ContentHash, DecisionId, RfcId, RfcState, SectionPath};
use orkia_rfc_lock::UnlockReason;

/// Every state-mutating operation emits one of these events. The shell layer
/// owns translation to its `event_router.on_custom(JobId(0), "orkia", "rfc.<name>", payload)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RfcEvent {
    Created {
        rfc_id: RfcId,
        by: AgentId,
    },
    StateChanged {
        rfc_id: RfcId,
        from: RfcState,
        to: RfcState,
        by: AgentId,
        reason: String,
    },
    Locked {
        rfc_id: RfcId,
        by: AgentId,
    },
    Unlocked {
        rfc_id: RfcId,
        by: AgentId,
        reason: SerdeUnlockReason,
    },
    EditApplied {
        rfc_id: RfcId,
        by: AgentId,
        section: SectionPath,
        prev_content_hash: ContentHash,
        new_content_hash: ContentHash,
    },
    Reopened {
        rfc_id: RfcId,
        by: AgentId,
        archived_version: u32,
        new_version: u32,
    },
    Promoted {
        rfc_id: RfcId,
        version: u32,
        approver: AgentId,
    },
    Completed {
        rfc_id: RfcId,
        by: AgentId,
    },
    Abandoned {
        rfc_id: RfcId,
        by: AgentId,
        reason: String,
    },
    DecisionOpened {
        rfc_id: RfcId,
        decision_id: DecisionId,
        by: AgentId,
    },
    DecisionResolved {
        rfc_id: RfcId,
        decision_id: DecisionId,
        by: AgentId,
    },
    DecisionProposed {
        rfc_id: RfcId,
        decision_id: DecisionId,
        by: AgentId,
    },
}

impl RfcEvent {
    /// Short event name used as the SEAL `name` field on `event_router.on_custom`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Created { .. } => "rfc.created",
            Self::StateChanged { .. } => "rfc.state_changed",
            Self::Locked { .. } => "rfc.locked",
            Self::Unlocked { .. } => "rfc.unlocked",
            Self::EditApplied { .. } => "rfc.edit_applied",
            Self::Reopened { .. } => "rfc.reopened",
            Self::Promoted { .. } => "rfc.promoted",
            Self::Completed { .. } => "rfc.completed",
            Self::Abandoned { .. } => "rfc.abandoned",
            Self::DecisionOpened { .. } => "rfc.decision_opened",
            Self::DecisionResolved { .. } => "rfc.decision_resolved",
            Self::DecisionProposed { .. } => "rfc.decision_proposed",
        }
    }

    /// RFC this event belongs to. Used by the SEAL consumer to tag the
    /// resulting record so the SEAL v1 assembler can collect every
    /// `rfc.*` event under the right document at closure
    pub fn rfc_id(&self) -> &RfcId {
        match self {
            Self::Created { rfc_id, .. }
            | Self::StateChanged { rfc_id, .. }
            | Self::Locked { rfc_id, .. }
            | Self::Unlocked { rfc_id, .. }
            | Self::EditApplied { rfc_id, .. }
            | Self::Reopened { rfc_id, .. }
            | Self::Promoted { rfc_id, .. }
            | Self::Completed { rfc_id, .. }
            | Self::Abandoned { rfc_id, .. }
            | Self::DecisionOpened { rfc_id, .. }
            | Self::DecisionResolved { rfc_id, .. }
            | Self::DecisionProposed { rfc_id, .. } => rfc_id,
        }
    }
}

/// Mirror of `orkia_rfc_lock::UnlockReason` that derives serde. Kept here to
/// avoid forcing serde into the lock crate's public API.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SerdeUnlockReason {
    AgentExit,
    Explicit,
    Timeout,
    Replaced,
}

impl From<UnlockReason> for SerdeUnlockReason {
    fn from(r: UnlockReason) -> Self {
        match r {
            UnlockReason::AgentExit => Self::AgentExit,
            UnlockReason::Explicit => Self::Explicit,
            UnlockReason::Timeout => Self::Timeout,
            UnlockReason::Replaced => Self::Replaced,
        }
    }
}

/// Pluggable sink. The shell layer wires this to `event_router.on_custom`;
/// tests use `RecordingSink`.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: RfcEvent);
}

/// In-memory recording sink for tests.
#[derive(Debug, Default)]
pub struct RecordingSink {
    events: Mutex<Vec<RfcEvent>>,
}

impl RecordingSink {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn drain(&self) -> Vec<RfcEvent> {
        match self.events.lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            Err(_) => Vec::new(),
        }
    }
}

impl EventSink for RecordingSink {
    fn emit(&self, event: RfcEvent) {
        if let Ok(mut g) = self.events.lock() {
            g.push(event);
        }
    }
}

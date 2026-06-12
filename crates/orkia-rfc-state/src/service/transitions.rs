// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

use orkia_rfc_core::{
    AgentId, RfcError, RfcId, RfcState, Transition, TransitionCtx, validate_transition,
};

use crate::events::RfcEvent;

use super::RfcStateService;

impl RfcStateService {
    // ── State transitions ──────────────────────────────────────────────

    pub fn promote(&self, id: &RfcId, approver: &AgentId) -> Result<RfcState, RfcError> {
        self.transition(id, Transition::Promote, approver, "promote")?;
        let rec = self.store.load(id)?;
        self.sink.emit(RfcEvent::Promoted {
            rfc_id: id.clone(),
            version: rec.fm.version,
            approver: approver.clone(),
        });
        Ok(rec.fm.state)
    }

    pub fn complete(&self, id: &RfcId, by: &AgentId) -> Result<RfcState, RfcError> {
        self.transition(id, Transition::Complete, by, "complete")?;
        self.sink.emit(RfcEvent::Completed {
            rfc_id: id.clone(),
            by: by.clone(),
        });
        Ok(RfcState::Completed)
    }

    pub fn abandon(&self, id: &RfcId, by: &AgentId, reason: &str) -> Result<RfcState, RfcError> {
        self.transition(id, Transition::Abandon, by, reason)?;
        self.sink.emit(RfcEvent::Abandoned {
            rfc_id: id.clone(),
            by: by.clone(),
            reason: reason.to_string(),
        });
        Ok(RfcState::Abandoned)
    }

    pub fn reopen(&self, id: &RfcId, by: &AgentId) -> Result<RfcState, RfcError> {
        let current = self.store.load(id)?;
        let from = current.fm.state;
        let ctx = TransitionCtx::default();
        // Validate that we *may* reopen from this state.
        validate_transition(id, from, Transition::Reopen, &ctx)?;
        let archived_version = current.fm.version;
        let new_rec = self.store.reopen(id)?;
        self.sink.emit(RfcEvent::Reopened {
            rfc_id: id.clone(),
            by: by.clone(),
            archived_version,
            new_version: new_rec.fm.version,
        });
        self.sink.emit(RfcEvent::StateChanged {
            rfc_id: id.clone(),
            from,
            to: RfcState::DraftActive,
            by: by.clone(),
            reason: "reopen".into(),
        });
        Ok(new_rec.fm.state)
    }

    // ── Internals ──────────────────────────────────────────────────────

    pub(super) fn transition(
        &self,
        id: &RfcId,
        t: Transition,
        by: &AgentId,
        reason: &str,
    ) -> Result<RfcState, RfcError> {
        let rec = self.store.load(id)?;
        let counts = self.store.decision_counts(id)?;
        let ctx = TransitionCtx {
            open_clarifications: counts.open_clarifications,
            unreviewed_decisions: counts.unreviewed_decisions,
            dispatch_done: true,
        };
        let from = rec.fm.state;
        let to = validate_transition(id, from, t, &ctx)?;
        let mut fm = rec.fm;
        fm.state = to;
        let _ = self.store.save(fm, rec.body)?;
        self.sink.emit(RfcEvent::StateChanged {
            rfc_id: id.clone(),
            from,
            to,
            by: by.clone(),
            reason: reason.to_string(),
        });
        Ok(to)
    }
}

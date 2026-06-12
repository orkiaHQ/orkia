// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

use chrono::Utc;

use orkia_rfc_core::{
    AgentId, DecisionId, DecisionKind, DecisionRecord, DecisionStatus, RfcError, RfcId, RfcState,
    RfcTool, Transition, tool_allowed,
};

use crate::events::RfcEvent;

use super::{AskRequest, EditRequest, LogDecisionRequest, RfcContext, RfcStateService};

impl RfcStateService {
    // ── Reads ──────────────────────────────────────────────────────────

    pub fn get_context(&self, id: &RfcId) -> Result<RfcContext, RfcError> {
        let rec = self.store.load(id)?;
        let counts = self.store.decision_counts(id)?;
        // Treat a poisoned mutex the same as `propose_edit` does: surface it
        // as an error rather than silently reporting the RFC as unlocked
        // (fail-open). (SEC-059)
        let locks = self.locks.lock().map_err(|_| RfcError::Io {
            operation: "lock_poison",
            message: "lock store mutex poisoned".into(),
        })?;
        let locked_by = locks.status(id).map(|i| i.held_by.clone());
        Ok(RfcContext {
            rfc_id: rec.fm.id,
            state: rec.fm.state,
            version: rec.fm.version,
            content: rec.body,
            content_hash: rec.fm.content_hash,
            locked_by,
            open_clarifications: counts.open_clarifications,
            unreviewed_decisions: counts.unreviewed_decisions,
        })
    }

    // ── Lifecycle: create ──────────────────────────────────────────────

    pub fn create(
        &self,
        id: &RfcId,
        by: &AgentId,
        title: Option<&str>,
    ) -> Result<RfcContext, RfcError> {
        let rec = self.store.create(id, title)?;
        self.sink.emit(RfcEvent::Created {
            rfc_id: id.clone(),
            by: by.clone(),
        });
        Ok(RfcContext {
            rfc_id: rec.fm.id,
            state: rec.fm.state,
            version: rec.fm.version,
            content: rec.body,
            content_hash: rec.fm.content_hash,
            locked_by: None,
            open_clarifications: 0,
            unreviewed_decisions: 0,
        })
    }
}

// ── Clarifications, decisions, edits ──────────────────────────────────────────

impl RfcStateService {
    pub fn ask(&self, req: AskRequest) -> Result<DecisionId, RfcError> {
        if req.rationale.trim().is_empty() {
            return Err(RfcError::RationaleRequired { operation: "ask" });
        }
        let rec = self.store.load(&req.rfc_id)?;
        tool_allowed(rec.fm.state, RfcTool::Ask).map_err(|action| RfcError::InvalidState {
            rfc_id: req.rfc_id.clone(),
            state: rec.fm.state,
            operation: "ask",
            action,
        })?;
        let did = self.next_decision_id();
        let rec_ts = Utc::now().fixed_offset();
        let record = DecisionRecord {
            id: did.clone(),
            rfc_id: req.rfc_id.clone(),
            rfc_version: rec.fm.version,
            ts: rec_ts,
            actor: req.agent.clone(),
            kind: DecisionKind::Clarification,
            content: serde_json::json!({ "q": req.question, "rationale": req.rationale }),
            status: DecisionStatus::Open,
            prev_hash: String::new(),
            hash: String::new(),
        };
        self.store.append_decision(&record)?;
        self.sink.emit(RfcEvent::DecisionOpened {
            rfc_id: req.rfc_id,
            decision_id: did.clone(),
            by: req.agent,
        });
        Ok(did)
    }

    /// Record the human's answer. If this was the last open clarification and
    pub fn resolve_clarification(
        &self,
        rfc_id: &RfcId,
        decision_id: &DecisionId,
        by: &AgentId,
        answer: &str,
    ) -> Result<(), RfcError> {
        let rec = self.store.load(rfc_id)?;

        // Verify the decision exists and is an *open* clarification before
        // appending a "resolved" event. Otherwise an unknown or already-resolved
        // id produces an orphan record while the call lies about success
        // (BUG-089). The decision log reuses one id across its lifecycle, so the
        // latest record for this id carries the current state.
        let decisions = self.store.read_decisions(rfc_id)?;
        let current = decisions.iter().rev().find(|d| &d.id == decision_id);
        match current {
            None => {
                return Err(RfcError::DecisionNotResolvable {
                    rfc_id: rfc_id.clone(),
                    decision_id: decision_id.clone(),
                    reason: "no such decision".into(),
                    action: "Check the decision id; only an open clarification can be resolved."
                        .into(),
                });
            }
            Some(d)
                if d.status != DecisionStatus::Open || d.kind != DecisionKind::Clarification =>
            {
                return Err(RfcError::DecisionNotResolvable {
                    rfc_id: rfc_id.clone(),
                    decision_id: decision_id.clone(),
                    reason: format!(
                        "decision is {:?}/{:?}, not an open clarification",
                        d.kind, d.status
                    ),
                    action: "Only an open clarification can be resolved.".into(),
                });
            }
            Some(_) => {}
        }

        let ts = Utc::now().fixed_offset();
        let record = DecisionRecord {
            id: decision_id.clone(),
            rfc_id: rfc_id.clone(),
            rfc_version: rec.fm.version,
            ts,
            actor: by.clone(),
            kind: DecisionKind::ClarificationResolved,
            content: serde_json::json!({ "a": answer }),
            status: DecisionStatus::Resolved,
            prev_hash: String::new(),
            hash: String::new(),
        };
        self.store.append_decision(&record)?;
        self.sink.emit(RfcEvent::DecisionResolved {
            rfc_id: rfc_id.clone(),
            decision_id: decision_id.clone(),
            by: by.clone(),
        });

        if rec.fm.state == RfcState::DraftEmpty {
            let counts = self.store.decision_counts(rfc_id)?;
            if counts.open_clarifications == 0 {
                self.transition(
                    rfc_id,
                    Transition::ResolveClarifications,
                    by,
                    "all clarifications resolved",
                )?;
            }
        }
        Ok(())
    }

    // ── Design decisions ───────────────────────────────────────────────

    pub fn log_decision(&self, req: LogDecisionRequest) -> Result<DecisionId, RfcError> {
        if req.rationale.trim().is_empty() {
            return Err(RfcError::RationaleRequired {
                operation: "log_decision",
            });
        }
        let rec = self.store.load(&req.rfc_id)?;
        tool_allowed(rec.fm.state, RfcTool::LogDecision).map_err(|action| {
            RfcError::InvalidState {
                rfc_id: req.rfc_id.clone(),
                state: rec.fm.state,
                operation: "log_decision",
                action,
            }
        })?;
        let did = self.next_decision_id();
        let record = DecisionRecord {
            id: did.clone(),
            rfc_id: req.rfc_id.clone(),
            rfc_version: rec.fm.version,
            ts: Utc::now().fixed_offset(),
            actor: req.agent.clone(),
            kind: DecisionKind::DesignProposed,
            content: serde_json::json!({
                "content": req.content,
                "rationale": req.rationale,
                "affects": req.affects,
            }),
            status: DecisionStatus::Proposed,
            prev_hash: String::new(),
            hash: String::new(),
        };
        self.store.append_decision(&record)?;
        self.sink.emit(RfcEvent::DecisionProposed {
            rfc_id: req.rfc_id,
            decision_id: did.clone(),
            by: req.agent,
        });
        Ok(did)
    }

    // ── Edits ──────────────────────────────────────────────────────────

    pub fn propose_edit(&self, req: EditRequest) -> Result<orkia_rfc_core::ContentHash, RfcError> {
        // Hold the lock-store mutex across the entire load → staleness-check →
        // save sequence. The old code loaded and checked the hash BEFORE
        // taking the lock and dropped it BEFORE saving, so two concurrent
        // edits could both pass `if_hash_matches` on the same snapshot and
        // clobber each other (lost update + a bogus `prev_content_hash` in the
        // audit trail). Serialising the read-modify-write closes that TOCTOU
        // window (BUG-035). `store::{load,save}` touch the filesystem, not this
        // mutex, so there's no re-entrancy.
        let mut locks = self.locks.lock().map_err(|_| RfcError::Io {
            operation: "lock_poison",
            message: "lock store mutex poisoned".into(),
        })?;

        let rec = self.store.load(&req.rfc_id)?;
        tool_allowed(rec.fm.state, RfcTool::ProposeEdit).map_err(|action| {
            RfcError::InvalidState {
                rfc_id: req.rfc_id.clone(),
                state: rec.fm.state,
                operation: "propose_edit",
                action,
            }
        })?;
        // no other writer can race past.
        if let Some(claimed) = &req.if_hash_matches {
            if claimed != &rec.fm.content_hash {
                return Err(RfcError::StaleSnapshot {
                    got: claimed.clone(),
                    expected: rec.fm.content_hash,
                    action: "Call orkia_rfc_get_context to refresh, then retry.".into(),
                });
            }
        }
        let acquired = locks.status(&req.rfc_id).is_none()
            || locks.status(&req.rfc_id).map(|i| &i.held_by) != Some(&req.agent);
        locks.acquire(&req.rfc_id, &req.agent)?;

        let prev_hash = rec.fm.content_hash.clone();
        let saved = self.store.save(rec.fm, req.new_body)?;
        drop(locks);

        if acquired {
            self.sink.emit(RfcEvent::Locked {
                rfc_id: req.rfc_id.clone(),
                by: req.agent.clone(),
            });
        }
        self.sink.emit(RfcEvent::EditApplied {
            rfc_id: req.rfc_id,
            by: req.agent,
            section: req.section,
            prev_content_hash: prev_hash,
            new_content_hash: saved.fm.content_hash.clone(),
        });
        Ok(saved.fm.content_hash)
    }
}

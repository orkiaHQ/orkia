// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use serde::{Deserialize, Serialize};

use crate::error::RfcError;
use crate::id::RfcId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RfcState {
    /// RFC just created. Body sections are write-locked. Agents may only ask
    /// clarification questions. Transitions out when all open clarifications
    /// are resolved.
    DraftEmpty,
    /// RFC has resolved clarifications and is being drafted. Agents may
    /// propose edits and log design decisions. Promotion requires approval.
    DraftActive,
    /// RFC is finalized for the current version. Read-mostly. Edits during
    /// Active require explicit human approval.
    Active,
    /// Previous version of an RFC that has been reopened. Read-only.
    Archived,
    /// Terminal: RFC has been successfully completed. May be reopened.
    Completed,
    /// Terminal: RFC has been abandoned with a reason. May be reopened.
    Abandoned,
}

impl RfcState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Abandoned | Self::Archived)
    }
    pub fn is_draft(self) -> bool {
        matches!(self, Self::DraftEmpty | Self::DraftActive)
    }
}

/// User-or-system intent driving a state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// System-triggered: the last open clarification was resolved.
    ResolveClarifications,
    /// Agent proposes; human approves.
    Promote,
    /// Human marks RFC as successfully completed.
    Complete,
    /// Human abandons the RFC with a reason.
    Abandon,
    /// Human reopens an Active/Completed/Abandoned RFC; current version is
    /// archived and a new DraftActive at v+1 is created.
    Reopen,
}

/// Context the validator needs in order to decide whether a transition is
/// allowed. Computed by the caller from filesystem state and passed in.
#[derive(Debug, Clone, Copy)]
pub struct TransitionCtx {
    /// Open clarification decisions on the current version.
    pub open_clarifications: u32,
    /// Logged design decisions that have not been reviewed yet.
    pub unreviewed_decisions: u32,
    /// Whether dispatch (if any) is complete. V1 has no dispatch, so the
    /// caller passes `true` unconditionally; V2 will gate on this.
    pub dispatch_done: bool,
}

impl Default for TransitionCtx {
    fn default() -> Self {
        Self {
            open_clarifications: 0,
            unreviewed_decisions: 0,
            dispatch_done: true,
        }
    }
}

/// `RfcError::InvalidState` with an educational action message on refusal.
///
/// Approval enforcement is *not* the responsibility of this function — it
/// checks structural eligibility only. The orkia-rfc-state service layer is
/// responsible for routing approval-gated transitions through the V3 prompt
/// before invoking this.
pub fn validate_transition(
    rfc_id: &RfcId,
    from: RfcState,
    t: Transition,
    ctx: &TransitionCtx,
) -> Result<RfcState, RfcError> {
    use RfcState::*;
    use Transition::*;
    let to = match (from, t) {
        (DraftEmpty, ResolveClarifications) => {
            if ctx.open_clarifications == 0 {
                DraftActive
            } else {
                return Err(RfcError::InvalidState {
                    rfc_id: rfc_id.clone(),
                    state: from,
                    operation: "resolve_clarifications",
                    action: format!(
                        "{} open clarifications remain. Resolve them before promoting.",
                        ctx.open_clarifications
                    ),
                });
            }
        }
        (DraftActive, Promote) => {
            if ctx.unreviewed_decisions > 0 {
                return Err(RfcError::InvalidState {
                    rfc_id: rfc_id.clone(),
                    state: from,
                    operation: "promote",
                    action: format!(
                        "{} design decisions await review. Use `rfc review` first.",
                        ctx.unreviewed_decisions
                    ),
                });
            }
            Active
        }
        (Active, Complete) => {
            if !ctx.dispatch_done {
                return Err(RfcError::InvalidState {
                    rfc_id: rfc_id.clone(),
                    state: from,
                    operation: "complete",
                    action: "Dispatch is still running. Wait for jobs to finish.".into(),
                });
            }
            Completed
        }
        (Active, Abandon) | (DraftActive, Abandon) => Abandoned,
        (Active, Reopen) | (Completed, Reopen) | (Abandoned, Reopen) => {
            // The caller is responsible for archiving the current version and
            // seeding a new DraftActive at v+1. The validator only confirms
            // the source state is reopenable.
            DraftActive
        }
        (state, _) => {
            return Err(RfcError::InvalidState {
                rfc_id: rfc_id.clone(),
                state,
                operation: transition_name(t),
                action: educational_hint(state, t),
            });
        }
    };
    Ok(to)
}

fn transition_name(t: Transition) -> &'static str {
    match t {
        Transition::ResolveClarifications => "resolve_clarifications",
        Transition::Promote => "promote",
        Transition::Complete => "complete",
        Transition::Abandon => "abandon",
        Transition::Reopen => "reopen",
    }
}

fn educational_hint(state: RfcState, t: Transition) -> String {
    match (state, t) {
        (RfcState::DraftEmpty, Transition::Promote) => {
            "RFC is in draft-empty. Use orkia_rfc_ask to gather requirements first.".into()
        }
        (RfcState::Archived, _) => {
            "This version is archived. Work on the current version instead.".into()
        }
        (RfcState::Completed, t) | (RfcState::Abandoned, t) if !matches!(t, Transition::Reopen) => {
            "RFC is in a terminal state. Use `rfc reopen` to start a new version.".into()
        }
        _ => "Operation not permitted from this state.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rid() -> RfcId {
        RfcId::new("test")
    }

    #[test]
    fn draft_empty_auto_promotes_when_clarifications_resolved() {
        let ctx = TransitionCtx::default();
        let to = validate_transition(
            &rid(),
            RfcState::DraftEmpty,
            Transition::ResolveClarifications,
            &ctx,
        );
        assert_eq!(to.ok(), Some(RfcState::DraftActive));
    }

    #[test]
    fn draft_empty_blocks_promote_on_open_clarifications() {
        let ctx = TransitionCtx {
            open_clarifications: 2,
            ..Default::default()
        };
        let r = validate_transition(
            &rid(),
            RfcState::DraftEmpty,
            Transition::ResolveClarifications,
            &ctx,
        );
        assert!(matches!(r, Err(RfcError::InvalidState { .. })));
    }

    #[test]
    fn promote_requires_reviewed_decisions() {
        let ctx = TransitionCtx {
            unreviewed_decisions: 1,
            ..Default::default()
        };
        assert!(matches!(
            validate_transition(&rid(), RfcState::DraftActive, Transition::Promote, &ctx),
            Err(RfcError::InvalidState { .. })
        ));
        let ctx = TransitionCtx::default();
        assert_eq!(
            validate_transition(&rid(), RfcState::DraftActive, Transition::Promote, &ctx).ok(),
            Some(RfcState::Active),
        );
    }

    #[test]
    fn complete_only_from_active() {
        let ctx = TransitionCtx::default();
        assert_eq!(
            validate_transition(&rid(), RfcState::Active, Transition::Complete, &ctx).ok(),
            Some(RfcState::Completed),
        );
        assert!(matches!(
            validate_transition(&rid(), RfcState::DraftActive, Transition::Complete, &ctx),
            Err(RfcError::InvalidState { .. })
        ));
    }

    #[test]
    fn abandon_from_drafts_and_active() {
        let ctx = TransitionCtx::default();
        for from in [RfcState::DraftActive, RfcState::Active] {
            assert_eq!(
                validate_transition(&rid(), from, Transition::Abandon, &ctx).ok(),
                Some(RfcState::Abandoned),
            );
        }
    }

    #[test]
    fn reopen_from_terminals() {
        let ctx = TransitionCtx::default();
        for from in [RfcState::Active, RfcState::Completed, RfcState::Abandoned] {
            assert_eq!(
                validate_transition(&rid(), from, Transition::Reopen, &ctx).ok(),
                Some(RfcState::DraftActive),
            );
        }
    }

    #[test]
    fn archived_refuses_everything() {
        let ctx = TransitionCtx::default();
        for t in [
            Transition::Promote,
            Transition::Complete,
            Transition::Abandon,
            Transition::Reopen,
        ] {
            assert!(matches!(
                validate_transition(&rid(), RfcState::Archived, t, &ctx),
                Err(RfcError::InvalidState { .. })
            ));
        }
    }
}

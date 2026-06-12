// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use crate::state::RfcState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RfcTool {
    GetContext,
    State,
    ListDecisions,
    Ask,
    LogDecision,
    ProposeEdit,
    ProposePromote,
}

impl RfcTool {
    pub fn name(self) -> &'static str {
        match self {
            Self::GetContext => "orkia_rfc_get_context",
            Self::State => "orkia_rfc_state",
            Self::ListDecisions => "orkia_rfc_list_decisions",
            Self::Ask => "orkia_rfc_ask",
            Self::LogDecision => "orkia_rfc_log_decision",
            Self::ProposeEdit => "orkia_rfc_propose_edit",
            Self::ProposePromote => "orkia_rfc_propose_promote",
        }
    }
}

/// Returns `Ok(())` if the tool is allowed in the given state, or
/// `Err(educational_hint)` describing why it's refused and what the agent
/// should do next.
pub fn tool_allowed(state: RfcState, tool: RfcTool) -> Result<(), String> {
    use RfcState::*;
    use RfcTool::*;
    // Reads are always allowed.
    if matches!(tool, GetContext | State | ListDecisions) {
        return Ok(());
    }
    match (state, tool) {
        (DraftEmpty | DraftActive, Ask) => Ok(()),
        (Active, Ask) => Err(
            "Active RFCs do not accept ad-hoc clarifications. Use job-level questions instead."
                .into(),
        ),
        (DraftEmpty, LogDecision) => {
            Err("RFC is in draft-empty. Use orkia_rfc_ask to gather requirements first.".into())
        }
        (DraftActive, LogDecision) => Ok(()),
        (Active, LogDecision) => Err(
            "Design decisions during Active are tracked at the job level, not on the RFC.".into(),
        ),
        (DraftEmpty, ProposeEdit) => {
            Err("RFC in draft-empty. Body sections locked. Resolve clarifications first.".into())
        }
        (DraftActive, ProposeEdit) => Ok(()),
        (Active, ProposeEdit) => Err(
            "Active RFCs are soft-locked. Use orkia_rfc_propose_amendment (see DISPATCH spec)."
                .into(),
        ),
        (DraftActive, ProposePromote) => Ok(()),
        (state, _) => Err(format!(
            "Operation not permitted in {state:?}. Read-only access via get_context."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_always_allowed() {
        for s in [
            RfcState::DraftEmpty,
            RfcState::DraftActive,
            RfcState::Active,
            RfcState::Archived,
            RfcState::Completed,
            RfcState::Abandoned,
        ] {
            assert!(tool_allowed(s, RfcTool::GetContext).is_ok());
            assert!(tool_allowed(s, RfcTool::State).is_ok());
            assert!(tool_allowed(s, RfcTool::ListDecisions).is_ok());
        }
    }

    #[test]
    fn ask_only_in_drafts() {
        assert!(tool_allowed(RfcState::DraftEmpty, RfcTool::Ask).is_ok());
        assert!(tool_allowed(RfcState::DraftActive, RfcTool::Ask).is_ok());
        assert!(tool_allowed(RfcState::Active, RfcTool::Ask).is_err());
        assert!(tool_allowed(RfcState::Archived, RfcTool::Ask).is_err());
    }

    #[test]
    fn propose_edit_only_in_draft_active() {
        assert!(tool_allowed(RfcState::DraftEmpty, RfcTool::ProposeEdit).is_err());
        assert!(tool_allowed(RfcState::DraftActive, RfcTool::ProposeEdit).is_ok());
        assert!(tool_allowed(RfcState::Active, RfcTool::ProposeEdit).is_err());
    }

    #[test]
    fn log_decision_only_in_draft_active() {
        assert!(tool_allowed(RfcState::DraftEmpty, RfcTool::LogDecision).is_err());
        assert!(tool_allowed(RfcState::DraftActive, RfcTool::LogDecision).is_ok());
        assert!(tool_allowed(RfcState::Active, RfcTool::LogDecision).is_err());
    }

    #[test]
    fn educational_action_present() {
        for tool in [RfcTool::Ask, RfcTool::LogDecision, RfcTool::ProposeEdit] {
            let r = tool_allowed(RfcState::Archived, tool);
            let msg = r.unwrap_err();
            assert!(
                !msg.is_empty(),
                "every refusal must include educational guidance"
            );
        }
    }
}

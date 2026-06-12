// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::decision::DecisionId;
use crate::hash::ContentHash;
use crate::id::{AgentId, RfcId, SectionPath};
use crate::state::RfcState;

/// Every variant carries an `action` field with educational guidance for the
/// conversation by reading the hint.
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum RfcError {
    #[error("RFC {rfc_id} not found")]
    NotFound { rfc_id: RfcId, operation: String },

    #[error("RFC {rfc_id} is in state {state:?}, operation {operation} not permitted: {action}")]
    InvalidState {
        rfc_id: RfcId,
        state: RfcState,
        operation: &'static str,
        action: String,
    },

    #[error("Cannot edit: {rfc_id} is locked by {locked_by}. {action}")]
    Locked {
        rfc_id: RfcId,
        locked_by: AgentId,
        action: String,
    },

    #[error("Stale snapshot: your hash {got:?}, current {expected:?}. {action}")]
    StaleSnapshot {
        got: ContentHash,
        expected: ContentHash,
        action: String,
    },

    #[error("Rationale required for {operation}")]
    RationaleRequired { operation: &'static str },

    #[error("Cannot propose edit to section {section}: {reason}. {action}")]
    SectionGuarded {
        section: SectionPath,
        reason: String,
        action: String,
    },

    #[error("I/O error during {operation}: {message}")]
    Io {
        operation: &'static str,
        message: String,
    },

    #[error("Frontmatter parse error: {message}")]
    Frontmatter { message: String },

    #[error("decision {decision_id} on RFC {rfc_id} cannot be resolved: {reason}. {action}")]
    DecisionNotResolvable {
        rfc_id: RfcId,
        decision_id: DecisionId,
        reason: String,
        action: String,
    },
}

impl RfcError {
    pub fn io(operation: &'static str, e: impl std::fmt::Display) -> Self {
        Self::Io {
            operation,
            message: e.to_string(),
        }
    }
}

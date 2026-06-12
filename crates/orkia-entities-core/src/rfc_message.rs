// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::enums::{RfcMessageAuthorType, RfcMessageType, RfcMessageValidatorStatus};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RfcMessageCore {
    pub id: Uuid,
    pub rfc_id: Uuid,
    pub workspace_id: Uuid,
    pub author_type: RfcMessageAuthorType,
    pub author_account_id: Option<Uuid>,
    pub author_agent_id: Option<Uuid>,
    pub body: String,
    pub message_type: RfcMessageType,
    pub metadata: Option<serde_json::Value>,
    pub parent_message_id: Option<Uuid>,
    pub sort_order: f64,
    pub created_at: DateTime<FixedOffset>,
    pub validator_status: Option<RfcMessageValidatorStatus>,
    pub validator_feedback: Option<serde_json::Value>,
    pub revised_from_message_id: Option<Uuid>,
    pub revision_explanation: Option<String>,
    pub revision_accepted_at: Option<DateTime<FixedOffset>>,
    pub revision_kept_draft_at: Option<DateTime<FixedOffset>>,
}

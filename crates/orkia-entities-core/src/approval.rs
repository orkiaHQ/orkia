// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::enums::ApprovalStatus;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovalCore {
    pub id: Uuid,
    pub issue_id: Uuid,
    pub workspace_id: Option<Uuid>,
    pub requested_by_agent_id: Uuid,
    pub routed_to_account_id: Uuid,
    pub approval_type: String,
    pub target: String,
    pub description: String,
    pub status: ApprovalStatus,
    pub files: Option<i32>,
    pub additions: Option<i32>,
    pub deletions: Option<i32>,
    pub timeout_seconds: i32,
    pub created_at: DateTime<FixedOffset>,
    pub resolved_at: Option<DateTime<FixedOffset>>,
    pub resolved_by: Option<Uuid>,
    pub updated_at: Option<DateTime<FixedOffset>>,
}

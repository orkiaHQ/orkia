// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::enums::BranchStatus;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IssueBranchCore {
    pub id: Uuid,
    pub source_issue_id: Uuid,
    pub parent_branch_id: Option<Uuid>,
    pub workspace_id: Uuid,
    pub title: String,
    pub description: Option<String>,
    pub status: BranchStatus,
    pub merge_selections: Option<serde_json::Value>,
    pub created_by: Option<Uuid>,
    pub visibility: String,
    pub merged_by: Option<Uuid>,
    pub merge_summary: Option<String>,
    pub created_at: DateTime<FixedOffset>,
    pub merged_at: Option<DateTime<FixedOffset>>,
    pub updated_at: DateTime<FixedOffset>,
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IssueCommentCore {
    pub id: Uuid,
    pub issue_id: Option<Uuid>,
    pub branch_id: Option<Uuid>,
    pub workspace_id: Option<Uuid>,
    pub author_account_id: Option<Uuid>,
    pub author_agent_id: Option<Uuid>,
    pub body: String,
    pub comment_type: Option<String>,
    pub attachments: serde_json::Value,
    pub merged_from_branch_id: Option<Uuid>,
    pub created_at: DateTime<FixedOffset>,
}

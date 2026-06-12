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
pub struct SealRecordCore {
    pub id: Uuid,
    pub issue_id: Option<Uuid>,
    pub branch_id: Option<Uuid>,
    pub action_type: String,
    pub detail: String,
    pub hash_chain: String,
    pub signature: Option<String>,
    pub journal_summary: Option<serde_json::Value>,
    pub parent_seal_id: Option<Uuid>,
    pub sealed_at: DateTime<FixedOffset>,
    pub workspace_id: Option<Uuid>,
    pub rfc_id: Option<Uuid>,
}

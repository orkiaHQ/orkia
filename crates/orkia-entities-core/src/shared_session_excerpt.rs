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
pub struct SharedSessionExcerptCore {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub source_session_id: Uuid,
    pub shared_by_account_id: Uuid,
    pub issue_id: Option<Uuid>,
    pub title: String,
    pub provider: String,
    pub content: serde_json::Value,
    pub note: String,
    pub shared_at: DateTime<FixedOffset>,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
}

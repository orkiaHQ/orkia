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
pub struct ProjectCore {
    pub id: Uuid,
    pub workspace_id: Option<Uuid>,
    pub origin_workspace_id: Uuid,
    pub name: String,
    pub identifier: Option<String>,
    pub color: Option<String>,
    pub archived: Option<bool>,
    pub status_columns: serde_json::Value,
    pub settings: serde_json::Value,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
}

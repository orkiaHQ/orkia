// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::enums::{ArchetypeOrigin, ArchetypeVisibility};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArchetypeCore {
    pub id: Uuid,
    pub namespace: String,
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub system_prompt: Option<String>,
    pub default_config: serde_json::Value,
    pub origin: ArchetypeOrigin,
    pub visibility: ArchetypeVisibility,
    pub workspace_id: Option<Uuid>,
    pub org_id: Option<Uuid>,
    pub created_by_account_id: Option<Uuid>,
    pub forked_from_id: Option<Uuid>,
    pub published_at: Option<DateTime<FixedOffset>>,
    pub archived: bool,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
}

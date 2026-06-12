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
pub struct WorkspaceCore {
    pub id: Uuid,
    pub owner_account_id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub slug: String,
    pub settings: serde_json::Value,
    /// `#[serde(default)]` keeps pre-migration sync payloads deserializable.
    #[serde(default)]
    pub home_rfc_id: Option<Uuid>,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
}

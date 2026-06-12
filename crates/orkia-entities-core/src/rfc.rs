// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::enums::RfcStatus;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RfcCore {
    pub id: Uuid,
    pub project_id: Uuid,
    pub workspace_id: Uuid,
    pub title: String,
    pub status: RfcStatus,
    pub current_version_id: Option<Uuid>,
    pub intent_classification: Option<String>,
    pub confidence: Option<f64>,
    pub routing_decision: Option<serde_json::Value>,
    pub generation_readiness: Option<serde_json::Value>,
    pub generation_overrides: Option<serde_json::Value>,
    pub ready_vote: Option<String>,
    pub created_by: Option<Uuid>,
    pub sort_order: f64,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
}

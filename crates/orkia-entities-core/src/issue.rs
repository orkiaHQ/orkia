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
pub struct IssueCore {
    pub id: Uuid,
    pub identifier: Option<String>,
    pub workspace_id: Uuid,
    pub project_id: Option<Uuid>,
    pub parent_issue_id: Option<Uuid>,
    pub created_by: Option<String>,
    pub assigned_agent_id: Option<Uuid>,
    pub title: String,
    pub description: Option<String>,
    pub status: String,
    pub priority: String,
    pub label: Option<String>,
    pub sort_order: f64,
    pub working_on: Option<String>,
    pub auto_routed: bool,
    pub route_confidence: Option<f64>,
    pub model: Option<String>,
    pub tokens_cost: Option<String>,
    pub owner_account_id: Option<Uuid>,
    pub rfc_id: Option<Uuid>,
    pub source: String,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::enums::{AgentRuntimeMode, AgentStatus};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentCore {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    pub archetype_id: Uuid,
    pub avatar_seed: String,
    pub trust_score: f32,
    pub trust_dimensions: serde_json::Value,
    pub memory: serde_json::Value,
    pub config: serde_json::Value,
    pub status: AgentStatus,
    pub runtime_mode: AgentRuntimeMode,
    pub governance_policy: serde_json::Value,
    pub custom_instructions: Option<String>,
    pub scope: Option<String>,
    pub max_steps_per_run: Option<i32>,
    pub temperature: Option<f32>,
    pub reinforcement_mode: Option<String>,
    pub llm: serde_json::Value,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
}

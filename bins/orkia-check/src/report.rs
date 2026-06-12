// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! JSON report schema (`version: "1"`).
//!
//! field change must bump the `version` literal.

use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Pass,
    Fail,
    Error,
}

// but no flow constructs them yet. Kept present so the schema is stable.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FlowStatus {
    Pass,
    Fail,
    Skipped,
    Errored,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModeOut {
    Local,
    Compose,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InfraState {
    Running,
    Ok,
    Unknown,
    Unreachable,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub errored: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct FailureDetail {
    pub code: String,
    pub message: String,
    pub expected: String,
    pub actual: String,
    pub hypothesis: String,
    pub logs_at: String,
    pub rendered_output_excerpt: String,
    pub related_specs: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FlowReport {
    pub id: String,
    pub name: String,
    pub status: FlowStatus,
    pub duration_ms: u64,
    /// Env group this flow ran in (`free`, `solo-pro`, …). Set by the
    /// runner after the flow returns; flow bodies leave it empty.
    pub env_group: String,
    pub stages_completed: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage_failed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureDetail>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InfraStatus {
    pub docker_compose: InfraState,
    pub backend_health: InfraState,
    pub postgres_health: InfraState,
}

impl InfraStatus {
    pub fn all_unknown() -> Self {
        Self {
            docker_compose: InfraState::Unknown,
            backend_health: InfraState::Unknown,
            postgres_health: InfraState::Unknown,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckReport {
    pub version: &'static str,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub mode: ModeOut,
    pub status: RunStatus,
    pub exit_code: i32,
    pub summary: Summary,
    pub flows: Vec<FlowReport>,
    pub failures: Vec<FailureDetail>,
    pub infrastructure: InfraStatus,
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use std::path::PathBuf;
use uuid::Uuid;

/// Runtime snapshot of an agent. Built from the on-disk
/// [`AgentDefinition`](crate::agent_def::AgentDefinition) plus a live
/// status (set by `JobController`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentInfo {
    pub id: Uuid,
    pub name: String,
    pub archetype: String,
    pub status: AgentStatus,
    pub model: String,

    /// Path to the agent directory on disk (`~/.orkia/agents/<name>/`).
    /// Empty for legacy/synthesized agents.
    #[serde(default)]
    pub dir: PathBuf,

    /// Optional short description from `agent.toml`.
    #[serde(default)]
    pub description: Option<String>,

    /// Binary the shell spawns for this agent (`claude`, `codex`, ...).
    #[serde(default)]
    pub command: String,

    /// CLI args appended to the command at spawn.
    #[serde(default)]
    pub args: Vec<String>,

    /// Projects whose RFCs/issues feed into the agent's spawn context.
    #[serde(default)]
    pub assigned_projects: Vec<String>,

    /// Context-window cap used when truncating the assembled bundle.
    #[serde(default)]
    pub max_context_tokens: usize,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum AgentStatus {
    Idle,
    Working,
    Waiting,
    Error,
}

// The legacy `TrustLevel` enum (a binned per-agent scalar) is removed
// Trust is per-(project × capability) — see `orkia_shell_types::trust`.

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryType {
    Shell,
    Intent,
    AgentDelegation,
    Builtin,
    Approval,
    Pipeline,
    ShellToAgent,
    AgentToSink,
}

impl HistoryType {
    pub fn short(&self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Intent => "intent",
            Self::AgentDelegation => "@agent",
            Self::Builtin => "builtin",
            Self::Approval => "approval",
            Self::Pipeline => "pipeline",
            Self::ShellToAgent => "sh|@",
            Self::AgentToSink => "@|sh",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub seq: u64,
    /// RFC3339 UTC timestamp.
    pub timestamp: String,
    pub entry_type: HistoryType,
    pub line: String,
    pub agent: Option<String>,
    pub job_id: Option<u32>,
}

impl HistoryEntry {
    pub fn new(seq: u64, entry_type: HistoryType, line: impl Into<String>) -> Self {
        Self {
            seq,
            timestamp: chrono::Utc::now().to_rfc3339(),
            entry_type,
            line: line.into(),
            agent: None,
            job_id: None,
        }
    }

    pub fn with_agent(mut self, agent: impl Into<String>) -> Self {
        self.agent = Some(agent.into());
        self
    }

    pub fn with_job(mut self, job_id: u32) -> Self {
        self.job_id = Some(job_id);
        self
    }

    /// Formatted HH:MM for display, falling back to "--:--" if the stored
    /// timestamp is not parseable.
    pub fn time_hhmm(&self) -> String {
        chrono::DateTime::parse_from_rfc3339(&self.timestamp)
            .map(|dt| dt.format("%H:%M").to_string())
            .unwrap_or_else(|_| "--:--".into())
    }
}

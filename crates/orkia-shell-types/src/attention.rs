// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Public vocabulary for the shell attention queue.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AttentionId(pub u64);

impl fmt::Display for AttentionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "attn-{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttentionKind {
    AgentPrompt,
    QueuedInput,
    BlockingApproval,
    ResourceConflict,
    AgentMessage,
}

impl AttentionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AgentPrompt => "agent_prompt",
            Self::QueuedInput => "queued_input",
            Self::BlockingApproval => "blocking_approval",
            Self::ResourceConflict => "resource_conflict",
            Self::AgentMessage => "agent_message",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AttentionSeverity {
    Fresh,
    Muted,
    Warning,
    Overdue,
    Blocking,
    Conflict,
}

impl AttentionSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Muted => "muted",
            Self::Warning => "warning",
            Self::Overdue => "overdue",
            Self::Blocking => "blocking",
            Self::Conflict => "conflict",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttentionAction {
    Pull,
    Resolve,
    Allow,
    Deny,
    Inspect,
    Hold,
    AbortAgent(String),
    ProceedAnyway,
}

impl AttentionAction {
    pub fn as_str(&self) -> String {
        match self {
            Self::Pull => "pull".into(),
            Self::Resolve => "resolve".into(),
            Self::Allow => "allow".into(),
            Self::Deny => "deny".into(),
            Self::Inspect => "inspect".into(),
            Self::Hold => "hold".into(),
            Self::AbortAgent(agent) => format!("abort-{agent}"),
            Self::ProceedAnyway => "proceed-anyway".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttentionRow {
    pub id: AttentionId,
    pub job_id: Option<u32>,
    pub agent: String,
    pub kind: AttentionKind,
    pub severity: AttentionSeverity,
    pub age: String,
    pub summary: String,
    pub actions: Vec<AttentionAction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttentionHint {
    Passive(String),
    Blocking { count: usize },
}

impl AttentionHint {
    pub fn render(&self) -> String {
        match self {
            Self::Passive(s) => s.clone(),
            Self::Blocking { count } => format!("[{count} blocking]"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttentionResolveEffect {
    None,
    HoldJob(u32),
    ReleaseJob(u32),
    StopJob(u32),
    Approval { job_id: u32, approved: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttentionCommandResult {
    pub rows: Vec<AttentionRow>,
    pub message: Option<String>,
    pub effect: AttentionResolveEffect,
}

pub trait AttentionControl: Send + Sync {
    fn pull(&self) -> AttentionCommandResult;
    fn resolve(&self, id: AttentionId, action: &str) -> AttentionCommandResult;
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use crate::agent::AgentInfo;

#[derive(Debug, Clone)]
pub struct RoutingDecision {
    pub agent_name: String,
    pub confidence: f32,
    pub reason: RoutingReason,
}

#[derive(Debug, Clone)]
pub enum RoutingReason {
    OnlyOption,
    ArchetypeMatch,
    DirectDelegation,
    Default,
}

pub trait AgentRouter: Send + Sync + 'static {
    fn route(&self, intent: &str, agents: &[AgentInfo]) -> Option<RoutingDecision>;
}

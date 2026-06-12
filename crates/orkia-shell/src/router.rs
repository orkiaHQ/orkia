// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

pub use orkia_shell_types::router::*;

use crate::agent::{AgentInfo, AgentStatus};

pub struct HeuristicRouter;

fn archetype_for_keyword(intent_lc: &str) -> Option<&'static str> {
    const RULES: &[(&[&str], &str)] = &[
        (
            &["fix", "implement", "code", "refactor", "bug"],
            "software-eng",
        ),
        (&["test", "qa", "coverage"], "qa-testing"),
        (&["deploy", "ci", "pipeline", "alert"], "devops"),
        (&["analyze", "report", "deck"], "business-ops"),
        (&["data", "metrics", "dashboard"], "data-analysis"),
    ];
    for (keywords, archetype) in RULES {
        if keywords.iter().any(|kw| intent_lc.contains(kw)) {
            return Some(archetype);
        }
    }
    None
}

impl AgentRouter for HeuristicRouter {
    fn route(&self, intent: &str, agents: &[AgentInfo]) -> Option<RoutingDecision> {
        if agents.is_empty() {
            return None;
        }
        if agents.len() == 1 {
            return Some(RoutingDecision {
                agent_name: agents[0].name.clone(),
                confidence: 0.6,
                reason: RoutingReason::OnlyOption,
            });
        }

        let intent_lc = intent.to_lowercase();
        if let Some(arch) = archetype_for_keyword(&intent_lc)
            && let Some(agent) = agents.iter().find(|a| a.archetype == arch)
        {
            return Some(RoutingDecision {
                agent_name: agent.name.clone(),
                confidence: 0.8,
                reason: RoutingReason::ArchetypeMatch,
            });
        }

        let fallback = agents
            .iter()
            .find(|a| a.status == AgentStatus::Idle)
            .unwrap_or(&agents[0]);
        Some(RoutingDecision {
            agent_name: fallback.name.clone(),
            confidence: 0.4,
            reason: RoutingReason::Default,
        })
    }
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

use orkia_shell_types::{AgentInfo, AgentStatus, BlockContent, Workspace};

pub fn render_briefing(
    agents: &[AgentInfo],
    workspace: &Workspace,
    seal_count: u64,
    _last_seal_hash: Option<&str>,
) -> Vec<BlockContent> {
    let mut blocks = vec![BlockContent::SystemInfo(format!(
        "⬡ orkia v{} · {} agents · SEAL: {} records",
        env!("CARGO_PKG_VERSION"),
        agents.len(),
        seal_count,
    ))];

    if !workspace.projects.is_empty() {
        let active_rfcs: usize = workspace
            .projects
            .iter()
            .flat_map(|p| &p.rfcs)
            .filter(|b| b.status == "active")
            .count();
        let open_issues: usize = workspace
            .projects
            .iter()
            .flat_map(|p| &p.issues)
            .filter(|i| i.status != "done")
            .count();

        blocks.push(BlockContent::SystemInfo(format!(
            "{} project(s) · {} active rfc(s) · {} open issue(s)",
            workspace.projects.len(),
            active_rfcs,
            open_issues,
        )));
    }

    let working = agents
        .iter()
        .filter(|a| a.status == AgentStatus::Working)
        .count();
    if working > 0 {
        blocks.push(BlockContent::SystemInfo(format!(
            "{working} agent(s) working"
        )));
    }

    blocks
}

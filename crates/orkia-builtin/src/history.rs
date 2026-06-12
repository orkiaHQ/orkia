// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use orkia_shell_types::{BlockContent, HistoryEntry, HistoryType};

#[derive(Debug, Default, Clone)]
pub struct HistoryQuery {
    pub limit: usize,
    pub only_shell: bool,
    pub only_agents: bool,
    pub only_approvals: bool,
    pub search: Option<String>,
}

impl HistoryQuery {
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut q = Self {
            limit: 20,
            ..Default::default()
        };
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "-n" | "--limit" => {
                    let n = it
                        .next()
                        .ok_or_else(|| "history: -n requires a value".to_string())?;
                    q.limit = n
                        .parse::<usize>()
                        .map_err(|_| format!("history: invalid -n value '{n}'"))?;
                }
                "--shell" => q.only_shell = true,
                "--agents" => q.only_agents = true,
                "--approvals" => q.only_approvals = true,
                "--search" => {
                    let needle = it
                        .next()
                        .ok_or_else(|| "history: --search requires a query".to_string())?;
                    q.search = Some(needle.clone());
                }
                other => return Err(format!("history: unknown flag '{other}'")),
            }
        }
        Ok(q)
    }

    pub fn matches(&self, entry: &HistoryEntry) -> bool {
        if self.only_shell && entry.entry_type != HistoryType::Shell {
            return false;
        }
        if self.only_agents
            && !matches!(
                entry.entry_type,
                HistoryType::Intent | HistoryType::AgentDelegation | HistoryType::Pipeline
            )
        {
            return false;
        }
        if self.only_approvals && entry.entry_type != HistoryType::Approval {
            return false;
        }
        if let Some(needle) = &self.search
            && !entry.line.contains(needle)
        {
            return false;
        }
        true
    }
}

pub fn render(entries: &[&HistoryEntry]) -> Vec<BlockContent> {
    if entries.is_empty() {
        return vec![BlockContent::SystemInfo("history is empty".into())];
    }
    let mut blocks = Vec::with_capacity(entries.len() + 1);
    blocks.push(BlockContent::SystemInfo(
        "  #     TIME   TYPE       COMMAND".into(),
    ));
    for entry in entries {
        blocks.push(BlockContent::Text(format!(
            "  {:<5} {:<6} {:<10} {}",
            entry.seq,
            entry.time_hhmm(),
            entry.entry_type.short(),
            entry.line,
        )));
    }
    blocks
}

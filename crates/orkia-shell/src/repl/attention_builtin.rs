// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;
use orkia_shell_types::AttentionControl;

impl Repl {
    pub(crate) fn dispatch_attention_effectful(&mut self, line: &str) -> Option<Outcome> {
        let tokens = tokenize_args(strip_command_prefix(line));
        if tokens.first().map(String::as_str) != Some("attention") {
            return None;
        }
        let sub = tokens.get(1).map(String::as_str).unwrap_or("list");
        match sub {
            "pull" => Some(Outcome::BuiltinOutput {
                blocks: attention_pull_blocks(&self.attention.pull()),
            }),
            "resolve" => {
                let id = parse_attention_id(tokens.get(2)?)?;
                let action = tokens.get(3)?.as_str();
                let result = self.attention.resolve(id, action);
                Some(self.apply_attention_result(result, action))
            }
            _ => None,
        }
    }

    fn apply_attention_result(
        &mut self,
        result: orkia_shell_types::AttentionCommandResult,
        action: &str,
    ) -> Outcome {
        match result.effect.clone() {
            orkia_shell_types::AttentionResolveEffect::Approval { job_id, approved } => {
                self.handle_resolution(&[job_id.to_string()], approved)
            }
            orkia_shell_types::AttentionResolveEffect::HoldJob(job_id) => {
                self.injection_executor.hold(JobId(job_id));
                Outcome::BuiltinOutput {
                    blocks: attention_result_blocks(&result),
                }
            }
            orkia_shell_types::AttentionResolveEffect::ReleaseJob(job_id) => {
                self.injection_executor.release(JobId(job_id));
                if action == "proceed-anyway" {
                    self.emit_attention_override(&result);
                }
                Outcome::BuiltinOutput {
                    blocks: attention_result_blocks(&result),
                }
            }
            orkia_shell_types::AttentionResolveEffect::StopJob(job_id) => {
                match self.jobs.stop(JobId(job_id)) {
                    Ok(()) => Outcome::BuiltinOutput {
                        blocks: vec![BlockContent::SystemInfo(format!(
                            "[{}] stopped by attention",
                            JobId(job_id)
                        ))],
                    },
                    Err(e) => Outcome::Error(format!("{e}")),
                }
            }
            orkia_shell_types::AttentionResolveEffect::None => {
                if action == "proceed-anyway" {
                    self.emit_attention_override(&result);
                }
                Outcome::BuiltinOutput {
                    blocks: attention_result_blocks(&result),
                }
            }
        }
    }

    fn emit_attention_override(&self, result: &orkia_shell_types::AttentionCommandResult) {
        let mut env = JournalEnvelope::now(EventType::Hook);
        env.event = Some("resource.override".into());
        env.source = Some("orkia".into());
        if let Some(row) = result.rows.first() {
            env.job_id = row.job_id;
            env.agent = Some(row.agent.clone());
            env.description = Some(row.summary.clone());
            env.action = Some(row.id.to_string());
        }
        self.emit_journal(env);
    }
}

fn strip_command_prefix(mut line: &str) -> &str {
    loop {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("orkia ") {
            line = rest;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('/') {
            line = rest;
            continue;
        }
        return trimmed;
    }
}

fn parse_attention_id(raw: &str) -> Option<orkia_shell_types::AttentionId> {
    let n = raw.strip_prefix("attn-").unwrap_or(raw).parse().ok()?;
    Some(orkia_shell_types::AttentionId(n))
}

fn attention_result_blocks(
    result: &orkia_shell_types::AttentionCommandResult,
) -> Vec<BlockContent> {
    let mut blocks = Vec::new();
    if let Some(message) = &result.message {
        blocks.push(BlockContent::SystemInfo(message.clone()));
    }
    blocks.extend(result.rows.iter().map(attention_row_block));
    if blocks.is_empty() {
        blocks.push(BlockContent::SystemInfo("attention queue is empty".into()));
    }
    blocks
}

fn attention_pull_blocks(result: &orkia_shell_types::AttentionCommandResult) -> Vec<BlockContent> {
    if !result.rows.is_empty() {
        return vec![BlockContent::Attention {
            rows: result.rows.clone(),
            message: result.message.clone(),
        }];
    }
    attention_result_blocks(result)
}

fn attention_row_block(row: &orkia_shell_types::AttentionRow) -> BlockContent {
    let actions = row
        .actions
        .iter()
        .map(action_label)
        .collect::<Vec<_>>()
        .join(", ");
    BlockContent::Text(format!(
        "{} job={} agent={} kind={} severity={} age={} actions=[{}]\n{}",
        row.id,
        row.job_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "-".into()),
        row.agent,
        row.kind.as_str(),
        row.severity.as_str(),
        row.age,
        actions,
        row.summary
    ))
}

fn action_label(action: &orkia_shell_types::AttentionAction) -> String {
    match action {
        orkia_shell_types::AttentionAction::Hold => {
            "hold (retain requester; defer Orkia-driven actions)".into()
        }
        orkia_shell_types::AttentionAction::AbortAgent(agent) => {
            format!("abort-{agent} (stop requester job)")
        }
        orkia_shell_types::AttentionAction::ProceedAnyway => {
            "proceed-anyway (override conflict and release requester)".into()
        }
        _ => action.as_str(),
    }
}

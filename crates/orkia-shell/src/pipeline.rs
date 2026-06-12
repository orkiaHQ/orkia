// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use crate::decision::PipelineStage;
use crate::error::ShellError;

pub fn parse_pipeline(line: &str) -> Result<Vec<PipelineStage>, ShellError> {
    let stages: Vec<&str> = line.split('|').collect();
    if stages.len() < 2 {
        return Err(ShellError::NotAPipeline);
    }
    stages
        .iter()
        .map(|s| {
            let trimmed = s.trim();
            if !trimmed.starts_with('@') {
                return Err(ShellError::PipelineMissingAgent(trimmed.to_string()));
            }
            let mut parts = trimmed[1..].splitn(2, char::is_whitespace);
            let agent = parts.next().unwrap_or("").to_string();
            let body = parts.next().unwrap_or("").trim().to_string();
            if agent.is_empty() {
                return Err(ShellError::PipelineMissingAgent(trimmed.to_string()));
            }
            Ok(PipelineStage { agent, body })
        })
        .collect()
}

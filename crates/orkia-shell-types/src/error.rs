// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    #[error("not a pipeline")]
    NotAPipeline,
    #[error("pipeline stage missing @agent: {0}")]
    PipelineMissingAgent(String),
    #[error("agent not found: {0}")]
    AgentNotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("job id space exhausted; cannot allocate a new job without recycling ids")]
    JobIdExhausted,
    #[error("{0}")]
    Other(String),
}

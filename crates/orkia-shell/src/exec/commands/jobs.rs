// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `jobs` — background shell jobs, migrated to a native `Command`.
//!
//! `Nothing → List(String)`. The cheapest migration of the wave: `CommandCtx.jobs`
//! is already populated (the `ps`/job-introspection snapshot), so this needs no
//! new context surface. Unlike `history` (a table), `jobs` is a **report**: it
//! has a bash-compatible line format (`[id]+ state label`) that is part of its
//! contract, so it emits the same lines as the legacy builtin — equivalence,
//! not a reshaped table. C1 holds (returns `PipelineData`, never `BlockContent`).

use async_trait::async_trait;
use orkia_shell_types::exec::command::{Command, CommandCtx, EvaluatedCall};
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{ExecError, JobInfo, JobKind, JobState, Signature, Type, Value};

pub struct Jobs;

#[async_trait]
impl Command for Jobs {
    fn signature(&self) -> Signature {
        Signature::builder("jobs")
            .io(Type::Nothing, Type::List(Box::new(Type::String)))
            .build()
    }

    fn description(&self) -> &str {
        "list background shell jobs"
    }

    fn is_streaming(&self) -> bool {
        false
    }

    async fn run(
        &self,
        ctx: &CommandCtx,
        _call: &EvaluatedCall,
        _input: PipelineData,
    ) -> Result<PipelineData, ExecError> {
        let shell_jobs: Vec<&JobInfo> = ctx
            .jobs
            .iter()
            .filter(|j| matches!(j.kind, JobKind::Shell { .. }))
            .collect();
        // bash semantics: the most recent shell job is `+` (current), the one
        // before it is `-` (previous).
        let current = shell_jobs.last().map(|j| j.id);
        let prev = shell_jobs.iter().rev().nth(1).map(|j| j.id);

        let lines: Vec<Value> = shell_jobs
            .iter()
            .map(|job| {
                let marker = if Some(job.id) == current {
                    "+"
                } else if Some(job.id) == prev {
                    "-"
                } else {
                    " "
                };
                Value::String(job_line(job, marker))
            })
            .collect();
        Ok(PipelineData::Value(Value::List(lines)))
    }
}

/// The bash-style line, identical to the legacy builtin's `[id]marker state label`.
fn job_line(job: &JobInfo, marker: &str) -> String {
    let state = match &job.state {
        JobState::Running | JobState::Foreground => "Running",
        JobState::Stopped => "Stopped",
        JobState::Done { .. } => "Done",
        JobState::Failed { .. } => "Failed",
    };
    format!(
        "  [{id}]{marker} {state:<10} {label}",
        id = job.id,
        label = job.label,
    )
}

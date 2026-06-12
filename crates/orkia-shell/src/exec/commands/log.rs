// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `log` — show a job's output log, migrated to a native `Command`.
//! état, preuve de l'enrichissement de `CommandCtx`).
//!
//! `Nothing → String`. Reads `<data_dir>/jobs/<id>/output.log` — the privileged
//! state now reachable because `CommandCtx` carries `data_dir` and the `jobs`
//! snapshot. The job-target resolution (`%N`) reuses the same standalone helper
//! as the legacy path (`builtin_resolve::resolve_job_target`), so behaviour is
//! identical. Output is plain text; the Display sink renders it (C1).

use async_trait::async_trait;
use orkia_shell_types::exec::command::{Command, CommandCtx, EvaluatedCall};
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{ExecError, FlagSpec, PositionalArg, Signature, Type, Value};

use crate::builtin_resolve::resolve_job_target;

pub struct Log;

#[async_trait]
impl Command for Log {
    fn signature(&self) -> Signature {
        Signature::builder("log")
            .io(Type::Nothing, Type::String)
            .required(PositionalArg::new(
                "target",
                Type::String,
                "job id, %N, or @agent",
            ))
            .flag(FlagSpec {
                long: "tail".to_string(),
                short: Some('n'),
                takes_arg: Some(Type::Int),
                desc: "show only the last N lines".to_string(),
            })
            .build()
    }

    fn description(&self) -> &str {
        "show a job's output log"
    }

    fn is_streaming(&self) -> bool {
        false
    }

    async fn run(
        &self,
        ctx: &CommandCtx,
        call: &EvaluatedCall,
        _input: PipelineData,
    ) -> Result<PipelineData, ExecError> {
        let target = match call.req(0)? {
            Value::String(s) => s.clone(),
            other => crate::exec::convert::scalar_to_string(other),
        };

        // Accept `%N`/`@agent` (resolved against the live job snapshot) and
        // bare ids (which survive even after the job is reaped — the log
        let id_num: u32 = if target.starts_with('%') || target.starts_with('@') {
            resolve_job_target(&target, &ctx.jobs)
                .map(|id| id.0)
                .ok_or_else(|| ExecError::BadArgs {
                    command: "log".to_string(),
                    message: format!("no job matching '{target}'"),
                })?
        } else {
            target.parse::<u32>().map_err(|_| ExecError::BadArgs {
                command: "log".to_string(),
                message: format!("invalid target '{target}'"),
            })?
        };

        let path = ctx
            .data_dir
            .join("jobs")
            .join(id_num.to_string())
            .join("output.log");
        // Verified accessor: a native command must hold `fs_read` for the path
        ctx.require_fs_read("log", &path)?;
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ExecError::Runtime {
                command: "log".to_string(),
                message: format!("no output log for job {id_num} at {}: {e}", path.display()),
            })?;

        // `--tail N` keeps the last N lines (default: the whole file).
        let tail = match call.get_flag("tail") {
            Some(Value::Int(n)) if *n >= 0 => Some(*n as usize),
            _ => None,
        };
        let lines: Vec<&str> = content.lines().collect();
        let start = tail.map_or(0, |n| lines.len().saturating_sub(n));
        let slice = lines[start..].join("\n");

        Ok(PipelineData::Value(Value::String(slice)))
    }
}

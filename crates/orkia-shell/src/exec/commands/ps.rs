// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Proves an existing builtin can move to the typed model without regression.
//! Produces a `Nothing → Table` of the known agents (drawn from the read-only
//! `CommandCtx` snapshot), so it composes: `ps | where status == working`,
//! `ps | sort-by trust`. The legacy `BuiltinCmd::Ps` path and
//! `orkia_builtin::ps::ps` are retained unchanged — the two coexist, the
//! registry simply takes precedence.

use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use indexmap::IndexMap;
use orkia_shell_types::exec::command::{Command, CommandCtx, EvaluatedCall};
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{AgentInfo, AgentStatus, ExecError, Signature, Type, Value};

pub struct Ps;

#[async_trait]
impl Command for Ps {
    fn signature(&self) -> Signature {
        Signature::builder("ps")
            .io(Type::Nothing, Type::Table)
            .build()
    }

    fn description(&self) -> &str {
        "list agents as a structured table"
    }

    async fn run(
        &self,
        ctx: &CommandCtx,
        _call: &EvaluatedCall,
        _input: PipelineData,
    ) -> Result<PipelineData, ExecError> {
        let rows: Vec<Value> = ctx.agents.iter().map(agent_record).collect();
        Ok(PipelineData::ListStream(
            stream::iter(rows.into_iter().map(Ok)).boxed(),
        ))
    }
}

fn agent_record(agent: &AgentInfo) -> Value {
    let mut record = IndexMap::new();
    record.insert("name".to_string(), Value::String(agent.name.clone()));
    record.insert(
        "archetype".to_string(),
        Value::String(agent.archetype.clone()),
    );
    record.insert(
        "status".to_string(),
        Value::String(status_label(&agent.status).to_string()),
    );
    // real decision, so it is not a queryable column. Effective per-(project ×
    // capability) trust is surfaced by the `trust` builtin instead.
    record.insert("model".to_string(), Value::String(agent.model.clone()));
    Value::Record(record)
}

fn status_label(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Idle => "idle",
        AgentStatus::Working => "working",
        AgentStatus::Waiting => "waiting",
        AgentStatus::Error => "error",
    }
}

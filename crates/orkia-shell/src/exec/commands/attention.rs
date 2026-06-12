// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `attention` — global attention queue.

use async_trait::async_trait;
use indexmap::IndexMap;
use orkia_shell_types::attention::{
    AttentionAction, AttentionCommandResult, AttentionId, AttentionRow,
};
use orkia_shell_types::exec::command::{Command, CommandCtx, EvaluatedCall};
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{ExecError, PositionalArg, Signature, Type, Value};

pub struct Attention;

#[async_trait]
impl Command for Attention {
    fn signature(&self) -> Signature {
        Signature::builder("attention")
            .io(Type::Nothing, Type::Table)
            .optional(PositionalArg::new(
                "sub",
                Type::String,
                "subcommand: list (default), pull, resolve, or help",
            ))
            .optional(PositionalArg::new("id", Type::String, "attention id"))
            .optional(PositionalArg::new(
                "action",
                Type::String,
                "resolution action",
            ))
            .build()
    }

    fn description(&self) -> &str {
        "show pending agent prompts"
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
        let sub = match call.opt(0) {
            Some(Value::String(s)) => s.as_str(),
            _ => "list",
        };
        if matches!(sub, "help" | "--help" | "-h") {
            return Ok(PipelineData::Value(Value::List(vec![Value::String(
                "attention list | pull | resolve <id> <action>".to_string(),
            )])));
        }
        match sub {
            "list" => {
                let rows: Vec<Value> = ctx.attention.iter().map(attention_record).collect();
                Ok(PipelineData::Value(Value::List(rows)))
            }
            "pull" => {
                let Some(control) = ctx.attention_control.as_ref() else {
                    return Ok(message_result("attention coordinator unavailable"));
                };
                Ok(result_to_pipeline(control.pull()))
            }
            "resolve" => {
                let id = parse_id(call.opt(1)).ok_or_else(|| ExecError::BadArgs {
                    command: "attention".to_string(),
                    message: "usage: attention resolve <id> <action>".into(),
                })?;
                let action = value_str(call.opt(2)).ok_or_else(|| ExecError::BadArgs {
                    command: "attention".to_string(),
                    message: "usage: attention resolve <id> <action>".into(),
                })?;
                let Some(control) = ctx.attention_control.as_ref() else {
                    return Ok(message_result("attention coordinator unavailable"));
                };
                Ok(result_to_pipeline(control.resolve(id, action)))
            }
            _ => Err(ExecError::BadArgs {
                command: "attention".to_string(),
                message: format!("unknown sub '{sub}' (try 'attention list')"),
            }),
        }
    }
}

fn value_str(value: Option<&Value>) -> Option<&str> {
    match value {
        Some(Value::String(s)) => Some(s.as_str()),
        _ => None,
    }
}

fn parse_id(value: Option<&Value>) -> Option<AttentionId> {
    let raw = value_str(value)?;
    let n = raw
        .strip_prefix("attn-")
        .unwrap_or(raw)
        .parse::<u64>()
        .ok()?;
    Some(AttentionId(n))
}

fn result_to_pipeline(result: AttentionCommandResult) -> PipelineData {
    let mut values = Vec::new();
    if let Some(message) = result.message {
        values.push(Value::String(message));
    }
    values.extend(result.rows.iter().map(attention_record));
    if result.effect != orkia_shell_types::AttentionResolveEffect::None {
        values.push(Value::String(format!("effect: {:?}", result.effect)));
    }
    PipelineData::Value(Value::List(values))
}

fn message_result(message: &str) -> PipelineData {
    PipelineData::Value(Value::List(vec![Value::String(message.to_string())]))
}

fn attention_record(att: &AttentionRow) -> Value {
    let mut record = IndexMap::new();
    record.insert("id".to_string(), Value::String(att.id.to_string()));
    record.insert(
        "job".to_string(),
        att.job_id
            .map(|id| Value::Int(id as i64))
            .unwrap_or(Value::Nothing),
    );
    record.insert("agent".to_string(), Value::String(att.agent.clone()));
    record.insert(
        "kind".to_string(),
        Value::String(att.kind.as_str().to_string()),
    );
    record.insert(
        "severity".to_string(),
        Value::String(att.severity.as_str().to_string()),
    );
    record.insert("age".to_string(), Value::String(att.age.clone()));
    record.insert("summary".to_string(), Value::String(att.summary.clone()));
    record.insert(
        "actions".to_string(),
        Value::List(att.actions.iter().map(action_value).collect()),
    );
    Value::Record(record)
}

fn action_value(action: &AttentionAction) -> Value {
    Value::String(action.as_str())
}

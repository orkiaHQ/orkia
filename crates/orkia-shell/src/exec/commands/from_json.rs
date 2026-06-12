// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! `ByteStream → Table`. This is the only path by which raw bytes become
//! structured values: the boundary is fail-closed, so without an explicit
//! converter a `ByteStream` into a structured command is refused. A top-level
//! JSON array becomes a stream of rows; any other value becomes a single row.
//! Malformed JSON yields `BadValue`, never a panic.

use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use indexmap::IndexMap;
use orkia_shell_types::exec::command::{Command, CommandCtx, EvaluatedCall};
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{ExecError, PositionalArg, Signature, Type, Value};

pub struct FromJson;

#[async_trait]
impl Command for FromJson {
    fn signature(&self) -> Signature {
        Signature::builder("from")
            .io(Type::ByteStream, Type::Table)
            .required(PositionalArg::new(
                "format",
                Type::String,
                "input format (only `json` in V1)",
            ))
            .build()
    }

    fn description(&self) -> &str {
        "parse bytes into structured values"
    }

    fn is_streaming(&self) -> bool {
        false
    }

    async fn run(
        &self,
        _ctx: &CommandCtx,
        call: &EvaluatedCall,
        input: PipelineData,
    ) -> Result<PipelineData, ExecError> {
        let format = match call.req(0)? {
            Value::String(s) => s.as_str(),
            _ => "",
        };
        if format != "json" {
            return Err(ExecError::BadArgs {
                command: "from".to_string(),
                message: format!("unsupported format `{format}` (only `json` is supported)"),
            });
        }

        let bytes = match input.into_value().await? {
            Value::Binary(b) => b,
            Value::String(s) => s.into_bytes(),
            Value::Nothing => Vec::new(),
            other => crate::exec::convert::scalar_to_string(&other).into_bytes(),
        };

        let parsed: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|e| ExecError::BadValue {
                command: "from json".to_string(),
                message: e.to_string(),
            })?;

        let rows: Vec<Value> = match parsed {
            serde_json::Value::Array(items) => items.into_iter().map(json_to_value).collect(),
            other => vec![json_to_value(other)],
        };

        Ok(PipelineData::ListStream(
            stream::iter(rows.into_iter().map(Ok)).boxed(),
        ))
    }
}

/// Map a `serde_json::Value` into an Orkia `Value`.
fn json_to_value(json: serde_json::Value) -> Value {
    match json {
        serde_json::Value::Null => Value::Nothing,
        serde_json::Value::Bool(b) => Value::Bool(b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Float(n.as_f64().unwrap_or(0.0)),
        },
        serde_json::Value::String(s) => Value::String(s),
        serde_json::Value::Array(items) => {
            Value::List(items.into_iter().map(json_to_value).collect())
        }
        serde_json::Value::Object(map) => {
            let mut record = IndexMap::new();
            for (key, value) in map {
                record.insert(key, json_to_value(value));
            }
            Value::Record(record)
        }
    }
}

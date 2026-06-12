// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! `Table → Table`. The predicate is parsed once, before streaming; then each
//! upstream record is tested as it arrives and dropped or forwarded. Filtering
//! is lazy: combined with `first N`, the producer stops early.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::StreamExt;
use orkia_shell_types::exec::command::{Command, CommandCtx, EvaluatedCall};
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{ExecError, PositionalArg, Signature, Type, Value};

use crate::exec::commands::predicate::{self, Predicate};

pub struct Where;

#[async_trait]
impl Command for Where {
    fn signature(&self) -> Signature {
        Signature::builder("where")
            .io(Type::Table, Type::Table)
            .rest(PositionalArg::new(
                "predicate",
                Type::String,
                "filter expression, e.g. `size > 1mb and type == file`",
            ))
            .build()
    }

    fn description(&self) -> &str {
        "filter a table by a predicate"
    }

    async fn run(
        &self,
        _ctx: &CommandCtx,
        call: &EvaluatedCall,
        input: PipelineData,
    ) -> Result<PipelineData, ExecError> {
        let tokens: Vec<String> = call
            .positional
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect();

        let predicate = predicate::parse(&tokens).map_err(|message| ExecError::BadArgs {
            command: "where".to_string(),
            message,
        })?;
        let predicate = Arc::new(predicate);

        match input {
            PipelineData::ListStream(stream) => {
                let filtered = stream.filter_map(move |item| {
                    let predicate: Arc<Predicate> = predicate.clone();
                    async move {
                        match item {
                            Ok(value) if predicate::eval(&predicate, &value) => Some(Ok(value)),
                            Ok(_) => None,
                            Err(e) => Some(Err(e)),
                        }
                    }
                });
                Ok(PipelineData::ListStream(filtered.boxed()))
            }
            // The kernel guarantees a Table input; anything else is empty.
            _ => Ok(PipelineData::Empty),
        }
    }
}

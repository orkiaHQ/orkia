// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! `Table → Table`. Pulls N items from the upstream stream, then **drops** it.
//! The drop propagates cancellation: a pull-based producer (e.g. `ls /huge`)
//! simply stops being polled. This is what makes `ls /huge | first 10` not
//! list all of `/huge`.

use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use orkia_shell_types::exec::command::{Command, CommandCtx, EvaluatedCall};
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{ExecError, PositionalArg, Signature, Type, Value};

pub struct First;

#[async_trait]
impl Command for First {
    fn signature(&self) -> Signature {
        Signature::builder("first")
            .io(Type::Table, Type::Table)
            .optional(PositionalArg::new(
                "count",
                Type::Int,
                "number of rows to take (default 1)",
            ))
            .build()
    }

    fn description(&self) -> &str {
        "take the first N rows, stopping the upstream"
    }

    async fn run(
        &self,
        _ctx: &CommandCtx,
        call: &EvaluatedCall,
        input: PipelineData,
    ) -> Result<PipelineData, ExecError> {
        let count = match call.opt(0) {
            Some(Value::Int(n)) if *n >= 0 => *n as usize,
            _ => 1,
        };

        let mut taken = Vec::with_capacity(count);
        if let PipelineData::ListStream(mut upstream) = input {
            for _ in 0..count {
                match upstream.next().await {
                    Some(item) => taken.push(item?),
                    None => break,
                }
            }
            drop(upstream); // cancel the producer
        }

        Ok(PipelineData::ListStream(
            stream::iter(taken.into_iter().map(Ok)).boxed(),
        ))
    }
}

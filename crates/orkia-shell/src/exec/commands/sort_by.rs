// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! `Table → Table`, `is_streaming() == false`: it must consume all input
//! before it can emit. Sorts ascending by one or more columns; `--reverse`
//! flips the order. Incomparable / missing values sort as equal (stable).

use std::cmp::Ordering;

use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use orkia_shell_types::exec::command::{Command, CommandCtx, EvaluatedCall};
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{ExecError, FlagSpec, PositionalArg, Signature, Type, Value};

pub struct SortBy;

#[async_trait]
impl Command for SortBy {
    fn signature(&self) -> Signature {
        Signature::builder("sort-by")
            .io(Type::Table, Type::Table)
            .required(PositionalArg::new(
                "column",
                Type::String,
                "column(s) to sort by",
            ))
            .rest(PositionalArg::new(
                "more",
                Type::String,
                "additional columns",
            ))
            .flag(FlagSpec {
                long: "reverse".to_string(),
                short: Some('r'),
                takes_arg: None,
                desc: "descending order".to_string(),
            })
            .build()
    }

    fn description(&self) -> &str {
        "sort a table by column(s)"
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
        let columns: Vec<String> = call
            .positional
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        let reverse = call.has_flag("reverse");

        // Collecting: drain the whole upstream before emitting.
        let mut rows = match input.into_value().await? {
            Value::List(items) => items,
            other => vec![other],
        };

        rows.sort_by(|a, b| {
            let ordering = compare_by_columns(a, b, &columns);
            if reverse {
                ordering.reverse()
            } else {
                ordering
            }
        });

        Ok(PipelineData::ListStream(
            stream::iter(rows.into_iter().map(Ok)).boxed(),
        ))
    }
}

/// Compare two rows across the ordered column list; the first column that
/// yields a definite ordering wins. Missing / incomparable → `Equal`.
fn compare_by_columns(a: &Value, b: &Value, columns: &[String]) -> Ordering {
    for column in columns {
        let lhs = a.get_path(column);
        let rhs = b.get_path(column);
        if let (Some(lhs), Some(rhs)) = (lhs, rhs)
            && let Some(ordering) = lhs.compare(rhs)
            && ordering != Ordering::Equal
        {
            return ordering;
        }
    }
    Ordering::Equal
}

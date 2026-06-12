// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `journal` — query the event journal as a structured table, migrated to a
//! le mécanisme `data_dir`).
//!
//! `Nothing → Table`. Reads the on-disk mirror `<data_dir>/journal.jsonl` (the
//! `data_dir` mechanism, like `history`) and emits one `Record` per matching
//! envelope — the structured C1 form, rendered as a table by the Display sink.
//! The filter grammar (`--agent`/`--job`/`--type`/`--event`/`--last N`/…) and
//! the summary derivation are reused from the `journal` module, so behaviour
//! matches the legacy builtin.
//!
//! Reading the disk mirror is eventually-consistent (an envelope still in the
//! writer thread's channel may not appear until flushed) — a deliberate
//! tradeoff to avoid sharing the in-memory store across the one-owner boundary.

use async_trait::async_trait;
use indexmap::IndexMap;
use orkia_shell_types::exec::command::{Command, CommandCtx, EvaluatedCall};
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{ExecError, PositionalArg, Signature, Type, Value};

use crate::journal::{JournalEnvelope, JournalStore, ParsedJournalArgs, journal_help_text};
use crate::journal::{event_summary, event_type_label};

pub struct Journal;

#[async_trait]
impl Command for Journal {
    fn signature(&self) -> Signature {
        Signature::builder("journal")
            .io(Type::Nothing, Type::Table)
            .rest(PositionalArg::new(
                "args",
                Type::String,
                "filters (--agent X, --job N, --type T, --event E, --last N, --since …)",
            ))
            .build()
    }

    fn description(&self) -> &str {
        "query the event journal as a table"
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
        let args: Vec<String> = call
            .positional
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => crate::exec::convert::scalar_to_string(other),
            })
            .collect();

        let parsed = ParsedJournalArgs::parse(&args).map_err(|e| ExecError::BadArgs {
            command: "journal".to_string(),
            message: e,
        })?;

        // `--help` is a text blurb, not a table — return it as a single line.
        if parsed.help {
            return Ok(PipelineData::Value(Value::List(vec![Value::String(
                journal_help_text().to_string(),
            )])));
        }

        // Same filter + `--last N` truncation as `JournalStore::query`, over the
        // on-disk mirror.
        let entries = JournalStore::load_entries(&ctx.data_dir);
        let mut hits: Vec<&JournalEnvelope> = entries
            .iter()
            .filter(|e| parsed.filter.matches(e))
            .collect();
        if let Some(n) = parsed.filter.last_n
            && hits.len() > n
        {
            hits.drain(..hits.len() - n);
        }

        let rows: Vec<Value> = hits.into_iter().map(envelope_record).collect();
        Ok(PipelineData::Value(Value::List(rows)))
    }
}

/// One table row per envelope: the legacy columns as typed fields.
fn envelope_record(env: &JournalEnvelope) -> Value {
    let mut record = IndexMap::new();
    record.insert(
        "timestamp".to_string(),
        Value::String(env.timestamp.clone()),
    );
    record.insert(
        "type".to_string(),
        Value::String(event_type_label(env.event_type).to_string()),
    );
    record.insert(
        "job".to_string(),
        env.job_id
            .map(|n| Value::Int(n as i64))
            .unwrap_or(Value::Nothing),
    );
    record.insert(
        "agent".to_string(),
        Value::String(env.agent.clone().unwrap_or_else(|| "-".to_string())),
    );
    record.insert("summary".to_string(), Value::String(event_summary(env)));
    Value::Record(record)
}

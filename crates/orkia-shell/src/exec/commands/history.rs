// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `history` — recent command history as a structured table, migrated to a
//! le mécanisme `data_dir`, comme `log`).
//!
//! `Nothing → Table`. Reads the on-disk history mirror
//! (`<data_dir>/history.jsonl`) and emits one `Record` per entry — the *truer*
//! C1 form: structured `Value`s the Display sink renders as a table, not
//! pre-rendered text. The filter grammar (`-n`/`--shell`/`--agents`/
//! `--approvals`/`--search`) reuses `orkia_builtin::history::HistoryQuery` so
//! behaviour matches the legacy builtin exactly.

use async_trait::async_trait;
use indexmap::IndexMap;
use orkia_builtin::history::HistoryQuery;
use orkia_shell_types::exec::command::{Command, CommandCtx, EvaluatedCall};
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{ExecError, FlagSpec, HistoryEntry, Signature, Type, Value};

use crate::history::History;

pub struct HistoryCmd;

#[async_trait]
impl Command for HistoryCmd {
    fn signature(&self) -> Signature {
        Signature::builder("history")
            .io(Type::Nothing, Type::Table)
            .flag(FlagSpec {
                long: "limit".to_string(),
                short: Some('n'),
                takes_arg: Some(Type::Int),
                desc: "show only the last N entries (default 20)".to_string(),
            })
            .flag(FlagSpec {
                long: "shell".to_string(),
                short: None,
                takes_arg: None,
                desc: "only shell commands".to_string(),
            })
            .flag(FlagSpec {
                long: "agents".to_string(),
                short: None,
                takes_arg: None,
                desc: "only agent/intent/pipeline entries".to_string(),
            })
            .flag(FlagSpec {
                long: "approvals".to_string(),
                short: None,
                takes_arg: None,
                desc: "only approval entries".to_string(),
            })
            .flag(FlagSpec {
                long: "search".to_string(),
                short: None,
                takes_arg: Some(Type::String),
                desc: "substring filter on the command line".to_string(),
            })
            .build()
    }

    fn description(&self) -> &str {
        "show recent command history as a table"
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
        let limit = match call.get_flag("limit") {
            Some(Value::Int(n)) if *n >= 0 => *n as usize,
            _ => 20,
        };
        let search = match call.get_flag("search") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };
        let query = HistoryQuery {
            limit,
            only_shell: call.has_flag("shell"),
            only_agents: call.has_flag("agents"),
            only_approvals: call.has_flag("approvals"),
            search,
        };

        // Last `limit` matching entries, in chronological order — the same
        // walk as `History::filter`.
        let entries = History::load_entries(&ctx.data_dir);
        let mut rows: Vec<&HistoryEntry> = entries
            .iter()
            .rev()
            .filter(|e| query.matches(e))
            .take(query.limit)
            .collect();
        rows.reverse();

        let records: Vec<Value> = rows.into_iter().map(entry_record).collect();
        Ok(PipelineData::Value(Value::List(records)))
    }
}

/// One table row per history entry: the four legacy columns as typed fields.
fn entry_record(entry: &HistoryEntry) -> Value {
    let mut record = IndexMap::new();
    record.insert("seq".to_string(), Value::Int(entry.seq as i64));
    record.insert("time".to_string(), Value::String(entry.time_hhmm()));
    record.insert(
        "type".to_string(),
        Value::String(entry.entry_type.short().to_string()),
    );
    record.insert("command".to_string(), Value::String(entry.line.clone()));
    Value::Record(record)
}

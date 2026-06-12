// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! `Nothing → Table`. Emits a lazy `ListStream` of records
//! `{name, size: Filesize, modified: Date, type}` for a directory. Lazy by
//! construction: entries are read and `stat`'d one at a time as the stream is
//! pulled, so `ork ls /huge | first 10` does not stat all of `/huge`.
//!
//! Invoked namespaced (`ork ls` / `orkia ls`); bare `ls` stays POSIX.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use indexmap::IndexMap;
use orkia_shell_types::exec::command::{Command, CommandCtx, EvaluatedCall};
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{ExecError, PositionalArg, Signature, Type, Value};
use tokio::fs::DirEntry;

pub struct Ls;

#[async_trait]
impl Command for Ls {
    fn signature(&self) -> Signature {
        Signature::builder("ls")
            .io(Type::Nothing, Type::Table)
            .optional(PositionalArg::new(
                "path",
                Type::String,
                "directory to list (default: cwd)",
            ))
            .build()
    }

    fn description(&self) -> &str {
        "list a directory as a structured table"
    }

    async fn run(
        &self,
        ctx: &CommandCtx,
        call: &EvaluatedCall,
        _input: PipelineData,
    ) -> Result<PipelineData, ExecError> {
        let path = match call.opt(0) {
            Some(Value::String(p)) => ctx.cwd.join(p),
            _ => ctx.cwd.clone(),
        };

        let read_dir = tokio::fs::read_dir(&path)
            .await
            .map_err(|e| ExecError::Runtime {
                command: "ls".to_string(),
                message: format!("{}: {e}", path.display()),
            })?;

        // Lazy: each poll reads and stats the next entry. Dropping the stream
        // (e.g. by `first N`) stops the directory walk.
        let stream = stream::unfold(Some(read_dir), |state| async move {
            let mut read_dir = state?;
            match read_dir.next_entry().await {
                Ok(Some(entry)) => Some((Ok(entry_record(entry).await), Some(read_dir))),
                Ok(None) => None,
                Err(e) => Some((
                    Err(ExecError::Runtime {
                        command: "ls".to_string(),
                        message: e.to_string(),
                    }),
                    None,
                )),
            }
        });

        Ok(PipelineData::ListStream(stream.boxed()))
    }
}

/// Build a record for one directory entry, stat'ing it lazily.
async fn entry_record(entry: DirEntry) -> Value {
    let name = entry.file_name().to_string_lossy().into_owned();
    let (size, modified, kind) = match entry.metadata().await {
        Ok(meta) => {
            let modified = meta
                .modified()
                .ok()
                .map(|t| Value::Date(DateTime::<Utc>::from(t)))
                .unwrap_or(Value::Nothing);
            let kind = if meta.is_dir() {
                "dir"
            } else if meta.file_type().is_symlink() {
                "symlink"
            } else {
                "file"
            };
            (
                Value::Filesize(meta.len() as i64),
                modified,
                Value::String(kind.to_string()),
            )
        }
        Err(_) => (
            Value::Nothing,
            Value::Nothing,
            Value::String("?".to_string()),
        ),
    };

    let mut record = IndexMap::new();
    record.insert("name".to_string(), Value::String(name));
    record.insert("size".to_string(), size);
    record.insert("modified".to_string(), modified);
    record.insert("type".to_string(), kind);
    Value::Record(record)
}

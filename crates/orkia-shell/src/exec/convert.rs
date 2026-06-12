// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! When structured data meets an external/PTY command downstream, it is
//! serialized to a deterministic, line-oriented byte form, streamed chunk by
//! chunk (never materialized): a `Record` becomes one TSV line; a scalar its
//! text form; a `List` one line per element.
//!
//! The reverse direction (`Bytes → Value`) has no implicit path: it is
//! fail-closed and only happens through an explicit converter (`from json`).
//! The engine's type check refuses a `ByteStream` into a structured input.

use bytes::Bytes;
use futures::stream::{self, StreamExt};
use orkia_shell_types::Value;
use orkia_shell_types::exec::pipeline_data::{ByteStream, PipelineData};

/// Serialize pipeline data to a streamed, deterministic byte form.
pub fn value_to_bytes(data: PipelineData) -> ByteStream {
    match data {
        PipelineData::ByteStream(stream) => stream,
        PipelineData::Empty => stream::empty().boxed(),
        PipelineData::Value(value) => {
            let bytes = Bytes::from(value_to_lines(&value));
            stream::once(async move { Ok(bytes) }).boxed()
        }
        PipelineData::ListStream(stream) => stream
            .map(|item| item.map(|value| Bytes::from(value_to_lines(&value))))
            .boxed(),
    }
}

/// Escape a TSV cell so embedded tabs/newlines (and the backslash used to
/// escape them) can't break the line/column framing (BUG-093). Backslash first.
fn escape_tsv_cell(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

/// Render one value to its line-oriented byte text (may be multiple lines for
/// a `List`). A `Record` is a single tab-separated line.
fn value_to_lines(value: &Value) -> String {
    match value {
        Value::Record(map) => {
            let mut out = String::new();
            for (i, v) in map.values().enumerate() {
                if i > 0 {
                    out.push('\t');
                }
                // Escape so a cell containing a tab (column break) or newline
                // (fake row) — e.g. a filename from `ls` — can't corrupt the
                // TSV line/column framing (BUG-093).
                out.push_str(&escape_tsv_cell(&scalar_to_string(v)));
            }
            out.push('\n');
            out
        }
        Value::List(items) => {
            let mut out = String::new();
            for item in items {
                out.push_str(&value_to_lines(item));
            }
            out
        }
        scalar => {
            let mut out = scalar_to_string(scalar);
            out.push('\n');
            out
        }
    }
}

/// The deterministic text form of a scalar value. Nested `List`/`Record`
/// fall back to compact JSON (`Record` is an `IndexMap`, so column order —
/// and therefore the output — is stable).
pub(crate) fn scalar_to_string(value: &Value) -> String {
    match value {
        Value::Nothing => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) | Value::Filesize(i) | Value::Duration(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Date(d) => d.to_rfc3339(),
        Value::String(s) => s.clone(),
        Value::Binary(b) => String::from_utf8_lossy(b).into_owned(),
        Value::List(_) | Value::Record(_) => serde_json::to_string(value).unwrap_or_default(),
    }
}

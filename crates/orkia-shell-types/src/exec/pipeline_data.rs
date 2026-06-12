// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! The streamed pipe medium (`PipelineData`).
//!
//! `Value`, a lazy pull-based stream of `Value`s (`ListStream` — the
//! structured-streaming path), a raw `ByteStream` (external/PTY output), or
//! nothing. Streaming is async and pull-based: dropping a stream cancels its
//! producer, which is how `first N` stops an upstream early.
//!
//! `PipelineData` is deliberately **not** `Clone` and **not** serde: a stream
//! has exactly one consumer (invariant). `Value` itself stays fully
//! serde — the "no non-serde Value variant" invariant is about `Value`, not
//! about this runtime carrier.

use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt};

use crate::exec::error::ExecError;
use crate::exec::typ::Type;
use crate::exec::value::Value;

/// A lazy, pull-based stream of `Value`s — typically `Record`s (table rows).
pub type ValueStream = BoxStream<'static, Result<Value, ExecError>>;

/// A lazy stream of raw byte chunks — external command / PTY / agent output.
pub type ByteStream = BoxStream<'static, Result<Bytes, ExecError>>;

/// The data flowing between pipeline stages.
pub enum PipelineData {
    /// A single materialized value (`echo 42`, the result of `length`).
    Value(Value),
    /// A lazy stream of `Value`s — the structured-streaming medium.
    ListStream(ValueStream),
    /// A lazy stream of raw bytes — external commands, PTY, interactive agents.
    ByteStream(ByteStream),
    /// No output (pure side-effect command).
    Empty,
}

impl PipelineData {
    /// The static type of this data, for contract checking. A `ListStream`
    /// is reported as `Table` (the common case is a stream of records);
    /// downstream `accepts` treats `Table`/`List` covariantly.
    pub fn type_of(&self) -> Type {
        match self {
            PipelineData::Value(v) => v.type_of(),
            PipelineData::ListStream(_) => Type::Table,
            PipelineData::ByteStream(_) => Type::ByteStream,
            PipelineData::Empty => Type::Nothing,
        }
    }

    /// Drain this data into a single `Value`, collecting any stream. Used by
    /// collecting commands (`sort-by`, `length`) that must see all input
    /// before emitting. A `ListStream` collapses to a `Value::List`; a
    /// `ByteStream` collapses to `Value::Binary`; `Empty` to `Value::Nothing`.
    pub async fn into_value(self) -> Result<Value, ExecError> {
        match self {
            PipelineData::Value(v) => Ok(v),
            PipelineData::Empty => Ok(Value::Nothing),
            PipelineData::ListStream(mut stream) => {
                let mut items = Vec::new();
                while let Some(next) = stream.next().await {
                    items.push(next?);
                }
                Ok(Value::List(items))
            }
            PipelineData::ByteStream(mut stream) => {
                let mut buf = Vec::new();
                while let Some(next) = stream.next().await {
                    buf.extend_from_slice(&next?);
                }
                Ok(Value::Binary(buf))
            }
        }
    }
}

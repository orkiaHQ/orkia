// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! This module hosts the *types* of the typed/streamed command system:
//! the value model (`Value`), the type system (`Type`), the error type
//! (`ExecError`), literal parsing, and the parsed-pipeline shape (`ExecPlan`).
//! It is the dependency root for the plugin system and the agentic fallback.
//!
//! The engine, registry, and concrete commands live in `orkia-shell`; this
//! crate stays free of any runtime, runner, or global state.

pub mod capability;
pub mod command;
pub mod error;
pub mod literal;
pub mod pipeline_data;
pub mod plan;
pub mod signature;
pub mod typ;
pub mod value;

pub use capability::{CapabilityScope, CapabilitySet, Scope};
pub use command::{Command, CommandCtx, EvaluatedCall};
pub use error::ExecError;
pub use pipeline_data::{ByteStream, PipelineData, ValueStream};
pub use plan::{ExecPlan, ParsedStage};
pub use signature::{FlagSpec, PositionalArg, Signature, SignatureBuilder};
pub use typ::Type;
pub use value::Value;

#[cfg(test)]
mod tests {
    use super::value::Value;
    use chrono::{TimeZone, Utc};
    use indexmap::IndexMap;

    /// Every `Value` variant must round-trip through serde_json — the
    /// invariant that the TS bridge (V2) and event surfacing (V4) rely on.
    #[test]
    fn value_serde_round_trip() {
        let mut record = IndexMap::new();
        record.insert("name".to_string(), Value::String("a.rs".into()));
        record.insert("size".to_string(), Value::Filesize(2048));

        let cases = vec![
            Value::Nothing,
            Value::Bool(true),
            Value::Int(-7),
            Value::Float(3.5),
            Value::Filesize(1_048_576),
            Value::Duration(5_000_000_000),
            Value::Date(Utc.with_ymd_and_hms(2026, 5, 28, 1, 2, 3).unwrap()),
            Value::String("hello".into()),
            Value::Binary(vec![0, 1, 2, 255]),
            Value::List(vec![Value::Int(1), Value::Int(2)]),
            Value::Record(record),
        ];

        for value in cases {
            let json = serde_json::to_string(&value).unwrap();
            let back: Value = serde_json::from_str(&json).unwrap();
            assert_eq!(value, back, "round-trip failed for {value:?}");
        }
    }
}

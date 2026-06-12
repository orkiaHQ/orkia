// Copyright 2026 Orkia
// SPDX-License-Identifier: Apache-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Apache License 2.0; see https://www.apache.org/licenses/LICENSE-2.0
// for terms.

//! `orkia-plugin-sdk` — write Orkia plugins in Rust, compiled to `wasm32-wasip1`.
//!
//! A plugin is a Rust function annotated `#[orkia::command]`:
//!
//! ```ignore
//! use orkia_plugin_sdk::prelude::*;
//!
//! #[orkia::command]
//! fn where_big(input: Vec<Value>, args: Args) -> Result<Vec<Value>> {
//!     let min: i64 = args.get("min_size")?;
//!     Ok(input.into_iter()
//!         .filter(|row| matches!(row.get_path("size"), Some(Value::Filesize(n)) if *n >= min))
//!         .collect())
//! }
//! ```
//!
//! ```bash
//! cargo build --target wasm32-wasip1 --release
//! orkia plugin add ./target/wasm32-wasip1/release/where_big.wasm
//! ```
//!
//! The host can't tell a Rust plugin from a TS one: both speak the tagged
//! JSON of [`orkia_value`] over the same `run_wasi_json` path (stdin envelope →
//! stdout array). `Value` here **is** EXEC-CORE's `Value` (re-exported), so
//! there is no separate type and no drift. Governance is inherited unchanged:
//! total sandbox by default, capabilities via the manifest, effects via MCP.
#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::fmt;

use indexmap::IndexMap;

pub use orkia_plugin_sdk_macros::command;
pub use orkia_value::{Value, json_to_value, value_to_json};

/// The plugin-author result type: `Ok(rows)` or a message-carrying [`Error`].
pub type Result<T> = core::result::Result<T, Error>;

/// A plugin error — surfaced to the host as a non-zero exit (a typed
/// `PluginError` on the host side), never a silent empty output.
#[derive(Debug)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Error(s.to_string())
    }
}

/// Typed access to the command's named arguments (`--name <value>`), parsed
/// from the call envelope as rich [`Value`]s. Fail-closed: a missing or
/// mistyped argument is an `Err`, not a silent default.
pub struct Args {
    named: IndexMap<String, Value>,
}

impl Args {
    /// A required, typed argument. `Err` if absent or not coercible to `T`.
    pub fn get<T: FromArg>(&self, name: &str) -> Result<T> {
        let value = self
            .named
            .get(name)
            .ok_or_else(|| Error(format!("missing argument `{name}`")))?;
        T::from_value(value).ok_or_else(|| Error(format!("argument `{name}` has the wrong type")))
    }

    /// An optional, typed argument: `None` if absent or not coercible.
    pub fn get_opt<T: FromArg>(&self, name: &str) -> Option<T> {
        self.named.get(name).and_then(T::from_value)
    }

    /// The raw [`Value`] of an argument, if present.
    pub fn raw(&self, name: &str) -> Option<&Value> {
        self.named.get(name)
    }
}

/// Coercion from a boundary [`Value`] into a typed argument.
pub trait FromArg: Sized {
    fn from_value(value: &Value) -> Option<Self>;
}

impl FromArg for Value {
    fn from_value(value: &Value) -> Option<Self> {
        Some(value.clone())
    }
}

impl FromArg for i64 {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Int(n) | Value::Filesize(n) | Value::Duration(n) => Some(*n),
            _ => None,
        }
    }
}

impl FromArg for f64 {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Float(f) => Some(*f),
            Value::Int(n) | Value::Filesize(n) | Value::Duration(n) => Some(*n as f64),
            _ => None,
        }
    }
}

impl FromArg for bool {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

impl FromArg for String {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::String(s) => Some(s.clone()),
            _ => None,
        }
    }
}

/// Parse the `{input, call}` envelope from `envelope_json` into the command's
/// `(Vec<Value>, Args)`. Pure (no I/O) so it is host-unit-testable. Malformed
/// shapes degrade to empty rather than failing — a plugin can't crash the host.
pub fn parse_envelope(envelope_json: &str) -> (Vec<Value>, Args) {
    let envelope: serde_json::Value =
        serde_json::from_str(envelope_json).unwrap_or(serde_json::Value::Null);
    let rows: Vec<Value> = envelope
        .get("input")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(json_to_value).collect())
        .unwrap_or_default();
    let mut named = IndexMap::new();
    if let Some(obj) = envelope
        .get("call")
        .and_then(|c| c.get("named"))
        .and_then(|n| n.as_object())
    {
        for (k, v) in obj {
            named.insert(k.clone(), json_to_value(v));
        }
    }
    (rows, Args { named })
}

/// Serialize the command's output rows to the JSON array the host reads.
pub fn encode_output(rows: &[Value]) -> String {
    let json = serde_json::Value::Array(rows.iter().map(value_to_json).collect());
    serde_json::to_string(&json).unwrap_or_else(|_| "[]".to_string())
}

/// The WASM entry glue invoked by `#[orkia::command]`'s generated `main`: read
/// the envelope from stdin, run `f`, write the output to stdout. On
/// `Err`, write to stderr and exit non-zero so the host surfaces a typed error.
pub fn run_command<F>(f: F)
where
    F: FnOnce(Vec<Value>, Args) -> Result<Vec<Value>>,
{
    use std::io::{Read, Write};

    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        let _ = std::io::stderr().write_all(b"plugin: failed to read stdin");
        std::process::exit(1);
    }
    let (rows, args) = parse_envelope(&input);
    match f(rows, args) {
        Ok(out) => {
            let _ = std::io::stdout().write_all(encode_output(&out).as_bytes());
        }
        Err(e) => {
            let _ = std::io::stderr().write_all(format!("plugin error: {e}").as_bytes());
            std::process::exit(1);
        }
    }
}

/// The `orkia` path so `#[orkia::command]` resolves after `use prelude::*`.
pub mod orkia {
    pub use orkia_plugin_sdk_macros::command;
}

/// Everything a plugin author needs in one glob.
pub mod prelude {
    pub use crate::{Args, Error, FromArg, Result, Value, json_to_value, orkia, value_to_json};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_envelope_extracts_input_and_named() {
        // Mirrors the host's `{input, call}` envelope with tagged values.
        let env = r#"{
            "input": [ {"size": {"$filesize": 5000}}, {"size": {"$filesize": 100}} ],
            "call": { "positional": [], "named": { "min_size": {"$filesize": 1000} } }
        }"#;
        let (rows, args) = parse_envelope(env);
        assert_eq!(rows.len(), 2);
        // Rich type survived: the row's `size` is a Filesize, not a bare Int.
        assert!(matches!(
            rows[0].get_path("size"),
            Some(Value::Filesize(5000))
        ));
        // Typed arg access coerces the filesize to i64.
        let min: i64 = args.get("min_size").unwrap();
        assert_eq!(min, 1000);
    }

    #[test]
    fn missing_or_mistyped_arg_is_err() {
        let (_, args) = parse_envelope(r#"{"input":[],"call":{"named":{"k":"text"}}}"#);
        assert!(args.get::<i64>("absent").is_err());
        assert!(args.get::<i64>("k").is_err()); // String is not coercible to i64
        assert_eq!(args.get::<String>("k").unwrap(), "text");
    }

    #[test]
    fn encode_output_is_tagged_json_array() {
        let out = vec![Value::Filesize(2048), Value::Int(7)];
        let s = encode_output(&out);
        // Filesize stays tagged (indiscernible from the TS path); Int is plain.
        assert_eq!(s, r#"[{"$filesize":2048},7]"#);
    }

    #[test]
    fn malformed_envelope_degrades_to_empty() {
        let (rows, args) = parse_envelope("not json");
        assert!(rows.is_empty());
        assert!(args.raw("anything").is_none());
    }
}

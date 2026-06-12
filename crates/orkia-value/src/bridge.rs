// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! language-agnostic wire format), shared by the host and every plugin SDK.
//!
//! WASM only passes numbers, so a `Value` crosses the host↔guest boundary
//! serialized as JSON. The rich numeric/temporal types (`Filesize`,
//! `Duration`, `Date`, `Binary`) are encoded as **tagged objects** so the
//! distinction survives the round trip — a plain JSON `number` can't tell a
//! filesize from a count. A TS plugin (via `@orkia/value`) and a Rust plugin
//! (via `orkia-plugin-sdk`) both produce/consume this exact shape, so the host
//! cannot tell which language a plugin was written in (the polyglot goal).

use base64::Engine as _;
use chrono::{DateTime, SecondsFormat, Utc};
use indexmap::IndexMap;
use serde_json::{Map, Number, Value as Json};

use crate::value::Value;

const TAG_FILESIZE: &str = "$filesize";
const TAG_DURATION: &str = "$duration_ns";
const TAG_DATE: &str = "$date";
const TAG_BINARY: &str = "$binary";

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// Encode a `Value` as boundary JSON.
pub fn value_to_json(value: &Value) -> Json {
    match value {
        Value::Nothing => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(i) => Json::Number((*i).into()),
        Value::Float(f) => Number::from_f64(*f).map(Json::Number).unwrap_or(Json::Null),
        Value::Filesize(n) => tagged(TAG_FILESIZE, Json::Number((*n).into())),
        Value::Duration(n) => tagged(TAG_DURATION, Json::Number((*n).into())),
        Value::Date(d) => tagged(
            TAG_DATE,
            Json::String(d.to_rfc3339_opts(SecondsFormat::AutoSi, true)),
        ),
        Value::String(s) => Json::String(s.clone()),
        Value::Binary(bytes) => tagged(TAG_BINARY, Json::String(b64().encode(bytes))),
        Value::List(items) => Json::Array(items.iter().map(value_to_json).collect()),
        Value::Record(map) => {
            let mut obj = Map::with_capacity(map.len());
            for (k, v) in map {
                obj.insert(k.clone(), value_to_json(v));
            }
            Json::Object(obj)
        }
    }
}

/// Decode boundary JSON back into a `Value`. A one-key object
/// matching a known tag becomes the rich type; any other object is a `Record`.
/// Malformed tags degrade to a plain `Record` rather than failing — a plugin
/// can't crash the host with a bad shape.
pub fn json_to_value(json: &Json) -> Value {
    match json {
        Json::Null => Value::Nothing,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => number_to_value(n),
        Json::String(s) => Value::String(s.clone()),
        Json::Array(items) => Value::List(items.iter().map(json_to_value).collect()),
        Json::Object(map) => object_to_value(map),
    }
}

/// Convert a JSON number to a [`Value`].
///
/// Note (BUG-N05): a JSON integer outside `i64` range (a `u64` above
/// `i64::MAX`) does not fit `Value::Int` and falls through to `Value::Float`,
/// which loses integer precision. There is no `Value::UInt`, so this is an
/// accepted limitation rather than a bug; the `unwrap_or(0.0)` is dead-code
/// defense (without `arbitrary_precision`, `as_f64` always returns `Some`).
fn number_to_value(n: &Number) -> Value {
    if let Some(i) = n.as_i64() {
        Value::Int(i)
    } else {
        Value::Float(n.as_f64().unwrap_or(0.0))
    }
}

fn object_to_value(map: &Map<String, Json>) -> Value {
    if map.len() == 1
        && let Some(rich) = decode_tagged(map)
    {
        return rich;
    }
    let mut record = IndexMap::with_capacity(map.len());
    for (k, v) in map {
        record.insert(k.clone(), json_to_value(v));
    }
    Value::Record(record)
}

/// Decode a one-key tagged object into its rich `Value`, or `None` if the tag
/// or payload doesn't match (caller falls back to `Record`).
fn decode_tagged(map: &Map<String, Json>) -> Option<Value> {
    let (key, val) = map.iter().next()?;
    match key.as_str() {
        TAG_FILESIZE => val.as_i64().map(Value::Filesize),
        TAG_DURATION => val.as_i64().map(Value::Duration),
        TAG_DATE => val
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| Value::Date(dt.with_timezone(&Utc))),
        TAG_BINARY => val
            .as_str()
            .and_then(|s| b64().decode(s).ok())
            .map(Value::Binary),
        _ => None,
    }
}

fn tagged(tag: &str, payload: Json) -> Json {
    let mut obj = Map::with_capacity(1);
    obj.insert(tag.to_string(), payload);
    Json::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn round_trip(v: Value) -> Value {
        json_to_value(&value_to_json(&v))
    }

    #[test]
    fn all_variants_round_trip() {
        let mut rec = IndexMap::new();
        rec.insert("name".to_string(), Value::String("a.rs".into()));
        rec.insert("size".to_string(), Value::Filesize(2048));
        rec.insert(
            "when".to_string(),
            Value::Date(Utc.with_ymd_and_hms(2026, 5, 29, 1, 2, 3).unwrap()),
        );

        let cases = vec![
            Value::Nothing,
            Value::Bool(true),
            Value::Int(-7),
            Value::Float(3.5),
            Value::Filesize(1_048_576),
            Value::Duration(5_000_000_000),
            Value::Date(Utc.with_ymd_and_hms(2026, 5, 29, 12, 0, 0).unwrap()),
            Value::String("hello".into()),
            Value::Binary(vec![0, 1, 2, 255, 254]),
            Value::List(vec![Value::Int(1), Value::Filesize(10), Value::Nothing]),
            Value::Record(rec),
        ];
        for case in cases {
            assert_eq!(
                round_trip(case.clone()),
                case,
                "round-trip failed for {case:?}"
            );
        }
    }

    #[test]
    fn rich_types_are_distinct_from_int() {
        assert_eq!(round_trip(Value::Filesize(42)), Value::Filesize(42));
        assert_ne!(
            value_to_json(&Value::Filesize(42)),
            value_to_json(&Value::Int(42))
        );
    }

    #[test]
    fn filesize_json_shape() {
        assert_eq!(
            value_to_json(&Value::Filesize(1_048_576)),
            serde_json::json!({ "$filesize": 1_048_576 })
        );
        assert_eq!(
            value_to_json(&Value::Duration(5_000_000_000i64)),
            serde_json::json!({ "$duration_ns": 5_000_000_000i64 })
        );
    }

    #[test]
    fn plain_record_is_not_mistaken_for_tag() {
        let mut rec = IndexMap::new();
        rec.insert("$filesize".to_string(), Value::Int(1));
        rec.insert("other".to_string(), Value::Int(2));
        assert_eq!(round_trip(Value::Record(rec.clone())), Value::Record(rec));
    }

    #[test]
    fn malformed_tag_degrades_to_record() {
        let json = serde_json::json!({ "$filesize": "nope" });
        match json_to_value(&json) {
            Value::Record(_) => {}
            other => panic!("expected Record fallback, got {other:?}"),
        }
    }

    #[test]
    fn non_finite_float_does_not_panic() {
        assert_eq!(value_to_json(&Value::Float(f64::NAN)), Json::Null);
        assert_eq!(value_to_json(&Value::Float(f64::INFINITY)), Json::Null);
    }
}

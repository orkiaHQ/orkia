// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! Value conversions between Rust domain types and their SQLite TEXT form.
//!
//! Closed-domain enums are stored as their serde discriminant: a plain-string
//! enum (e.g. `TurnRole`) becomes the bare word `"user"`; a structured enum
//! (e.g. `TurnKind::ToolCall`) becomes its JSON object. [`enum_from_text`]
//! fails closed on an unknown discriminant so the caller can skip the row.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde::de::DeserializeOwned;
use uuid::Uuid;

use crate::StoreError;

/// Serialize a closed-domain enum to its TEXT column form.
pub(crate) fn enum_to_text<T: Serialize>(v: &T) -> Result<String, StoreError> {
    match serde_json::to_value(v)? {
        serde_json::Value::String(s) => Ok(s),
        other => Ok(other.to_string()),
    }
}

/// Parse a closed-domain enum from its TEXT column form. Tries the bare-string
/// reading first (plain enums), then the JSON-object reading (structured
/// enums). An unrecognized value yields `Err` — fail closed.
pub(crate) fn enum_from_text<T: DeserializeOwned>(s: &str) -> Result<T, StoreError> {
    let as_str = serde_json::Value::String(s.to_string());
    if let Ok(v) = serde_json::from_value::<T>(as_str) {
        return Ok(v);
    }
    serde_json::from_str(s).map_err(StoreError::from)
}

/// UUID → hyphenated TEXT.
pub(crate) fn uuid_text(id: Uuid) -> String {
    id.to_string()
}

/// hyphenated TEXT → UUID (fail closed on malformed input).
pub(crate) fn parse_uuid(s: &str) -> Result<Uuid, StoreError> {
    Uuid::parse_str(s).map_err(|e| StoreError::Corrupt(format!("invalid uuid {s:?}: {e}")))
}

/// `DateTime<Utc>` → RFC3339 TEXT.
pub(crate) fn ts_text(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339()
}

/// RFC3339 TEXT → `DateTime<Utc>` (fail closed on malformed input).
pub(crate) fn parse_ts(s: &str) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| StoreError::Corrupt(format!("invalid timestamp {s:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_reasoning_core::enums::{TurnKind, TurnRelation, TurnRole};

    #[test]
    fn plain_enum_stores_as_bare_word() {
        assert_eq!(enum_to_text(&TurnRole::User).unwrap(), "user");
        let back: TurnRole = enum_from_text("user").unwrap();
        assert_eq!(back, TurnRole::User);
    }

    #[test]
    fn structured_enum_round_trips_as_json() {
        let k = TurnKind::ToolCall("Bash".into());
        let text = enum_to_text(&k).unwrap();
        assert!(text.starts_with('{'));
        let back: TurnKind = enum_from_text(&text).unwrap();
        assert_eq!(back, k);
    }

    #[test]
    fn unknown_enum_fails_closed() {
        let r: Result<TurnRelation, _> = enum_from_text("teleport");
        assert!(r.is_err());
    }

    #[test]
    fn uuid_and_ts_round_trip() {
        let id = Uuid::from_u128(42);
        assert_eq!(parse_uuid(&uuid_text(id)).unwrap(), id);
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        assert_eq!(parse_ts(&ts_text(now)).unwrap(), now);
    }
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! The structured value model (`Value`).
//!
//! the TS bridge (V2) and event surfacing (V4). No variant may hold a
//! non-serde type (no `Box<dyn>`, handle, or fd). A "Table" is not a variant:
//! it is a list/stream of `Record`.

use std::cmp::Ordering;

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::typ::Type;

/// A structured pipeline value.
///
/// `Float` forces `PartialEq` only (no `Eq`/`Hash`). `Filesize` and
/// `Duration` are deliberately distinct from `Int` so `where size > 1mb`
/// compares typed quantities, not bare integers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Value {
    /// Absence of a value (≈ null) — the result of an empty filter, etc.
    Nothing,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// Size in bytes. Distinct from `Int`: enables `where size > 1mb`.
    Filesize(i64),
    /// Duration in nanoseconds. Distinct from `Int`: enables `where elapsed > 5sec`.
    Duration(i64),
    /// UTC date/time (RFC3339 on the wire via chrono's serde).
    Date(DateTime<Utc>),
    String(String),
    /// Raw binary data (binary output of a structured command).
    Binary(Vec<u8>),
    /// Heterogeneous list.
    List(Vec<Value>),
    /// Column-ordered record — one row of a table.
    Record(IndexMap<String, Value>),
}

impl Value {
    /// The structural type of this value (used for contract checking).
    pub fn type_of(&self) -> Type {
        match self {
            Value::Nothing => Type::Nothing,
            Value::Bool(_) => Type::Bool,
            Value::Int(_) => Type::Int,
            Value::Float(_) => Type::Float,
            Value::Filesize(_) => Type::Filesize,
            Value::Duration(_) => Type::Duration,
            Value::Date(_) => Type::Date,
            Value::String(_) => Type::String,
            Value::Binary(_) => Type::Binary,
            Value::List(items) if list_is_table(items) => Type::Table,
            // Non-table lists erase the element type to `Any` by design.
            // The type-checker does not distinguish `List(Int)` from
            // `List(String)` — all non-table lists are treated as `List(Any)`.
            // This is intentional, not an oversight.
            Value::List(_) => Type::List(Box::new(Type::Any)),
            Value::Record(_) => Type::Record(Vec::new()),
        }
    }

    /// Borrow the inner record, if this is a `Record`.
    pub fn as_record(&self) -> Option<&IndexMap<String, Value>> {
        match self {
            Value::Record(map) => Some(map),
            _ => None,
        }
    }

    /// Resolve a dotted column path (`status.phase`) against a record,
    /// descending into nested records. Returns `None` if any segment is
    /// missing or a non-record is encountered mid-path.
    pub fn get_path(&self, path: &str) -> Option<&Value> {
        let mut current = self;
        for segment in path.split('.') {
            current = current.as_record()?.get(segment)?;
        }
        Some(current)
    }

    /// Total-order-free comparison between two values. Returns `None` when
    /// the variants are not comparable (different kinds, or `NaN`). Used by
    /// `where` (filtering) and `sort-by` (ordering with a stable fallback).
    pub fn compare(&self, other: &Value) -> Option<Ordering> {
        match (self, other) {
            (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
            (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
            (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
            (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
            (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)),
            (Value::Filesize(a), Value::Filesize(b)) => Some(a.cmp(b)),
            (Value::Duration(a), Value::Duration(b)) => Some(a.cmp(b)),
            // A bare integer is comparable to a typed quantity (`size > 50`).
            (Value::Filesize(a), Value::Int(b)) | (Value::Int(a), Value::Filesize(b)) => {
                Some(a.cmp(b))
            }
            (Value::Duration(a), Value::Int(b)) | (Value::Int(a), Value::Duration(b)) => {
                Some(a.cmp(b))
            }
            (Value::Date(a), Value::Date(b)) => Some(a.cmp(b)),
            (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }
}

/// A list is a `Table` when it is non-empty and every element is a `Record`.
fn list_is_table(items: &[Value]) -> bool {
    !items.is_empty() && items.iter().all(|v| matches!(v, Value::Record(_)))
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! The pipeline type system (`Type`) and the contract-check primitive.
//!
//! pairs; the kernel checks, before running anything, that an upstream's
//! output type satisfies the downstream's input type. The check is
//! fail-closed: an unknown or structurally incompatible pair is refused.

use serde::{Deserialize, Serialize};

/// A pipeline value type. `Table` is sugar for a stream/list of `Record`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Type {
    /// Accepts anything (e.g. the input of `to json`, the output of `from json`).
    Any,
    Nothing,
    Bool,
    Int,
    Float,
    Filesize,
    Duration,
    Date,
    String,
    Binary,
    List(Box<Type>),
    /// Typed columns; an empty column list means "any record".
    Record(Vec<(String, Type)>),
    /// A stream/list of `Record` — sugar for `List(Record(_))`.
    Table,
    /// Raw bytes (PTY / external command output).
    ByteStream,
}

impl Type {
    /// Does a value of type `self` (a declared *input* slot) accept a value
    /// of type `got` (an upstream's actual *output*)?
    ///
    /// `Any` on either side is unknown and therefore always accepted — the
    /// type system cannot refuse what it cannot describe. Otherwise the rule
    /// is structural equality, with `List`/`Table` and `Record` covariance.
    /// Crucially, `ByteStream` does **not** satisfy a structured slot
    /// (`Table`/`List`/`Record`): that is the `kubectl | where` refusal and
    /// the generalization of the old `AgentOnLeft` rule.
    pub fn accepts(&self, got: &Type) -> bool {
        match (self, got) {
            (Type::Any, _) | (_, Type::Any) => true,
            (Type::Table, _) | (_, Type::Table) => {
                let table = Type::List(Box::new(Type::Record(Vec::new())));
                let lhs = if matches!(self, Type::Table) {
                    &table
                } else {
                    self
                };
                let rhs = if matches!(got, Type::Table) {
                    &table
                } else {
                    got
                };
                lhs.accepts(rhs)
            }
            (Type::List(inner_self), Type::List(inner_got)) => inner_self.accepts(inner_got),
            (Type::Record(cols_self), Type::Record(cols_got)) => {
                cols_self.is_empty() || cols_self == cols_got
            }
            (a, b) => a == b,
        }
    }
}

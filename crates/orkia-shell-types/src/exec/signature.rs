// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Declarative command signatures.
//!
//! `(input, output)` type pairs and its argument grammar. The kernel binds
//! raw args to the signature and type-checks the chain *before* calling
//! `run`, so a command author writes only the happy path.

use crate::exec::typ::Type;

/// A positional argument (required, optional, or the catch-all `rest`).
#[derive(Clone, Debug)]
pub struct PositionalArg {
    pub name: String,
    pub ty: Type,
    pub desc: String,
}

impl PositionalArg {
    pub fn new(name: &str, ty: Type, desc: &str) -> Self {
        Self {
            name: name.to_string(),
            ty,
            desc: desc.to_string(),
        }
    }
}

/// A named flag. `takes_arg` is `Some(ty)` for value flags (`--limit 10`),
/// `None` for boolean switches (`--full`).
#[derive(Clone, Debug)]
pub struct FlagSpec {
    pub long: String,
    pub short: Option<char>,
    pub takes_arg: Option<Type>,
    pub desc: String,
}

/// A command's declared interface: name, IO type pairs, and arg grammar.
#[derive(Clone, Debug)]
pub struct Signature {
    pub name: String,
    /// Accepted `(input, output)` pairs. Multiple pairs model overloads
    /// (e.g. `length` accepts `String→Int` and `Table→Int`).
    pub io_types: Vec<(Type, Type)>,
    pub required: Vec<PositionalArg>,
    pub optional: Vec<PositionalArg>,
    pub flags: Vec<FlagSpec>,
    pub rest: Option<PositionalArg>,
}

impl Signature {
    /// Start building a signature for `name`.
    pub fn builder(name: &str) -> SignatureBuilder {
        SignatureBuilder {
            inner: Signature {
                name: name.to_string(),
                io_types: Vec::new(),
                required: Vec::new(),
                optional: Vec::new(),
                flags: Vec::new(),
                rest: None,
            },
        }
    }

    /// The output type for a given upstream input type: the first `(in, out)`
    /// pair whose declared input `accepts` the actual input. `None` means the
    /// command does not accept that input type (a type mismatch).
    pub fn output_for(&self, input: &Type) -> Option<&Type> {
        self.io_types
            .iter()
            .find(|(decl_in, _)| decl_in.accepts(input))
            .map(|(_, out)| out)
    }
}

/// Builder for [`Signature`] — keeps construction under the 4-argument rule.
pub struct SignatureBuilder {
    inner: Signature,
}

impl SignatureBuilder {
    /// Add an accepted `(input, output)` type pair.
    pub fn io(mut self, input: Type, output: Type) -> Self {
        self.inner.io_types.push((input, output));
        self
    }

    pub fn required(mut self, arg: PositionalArg) -> Self {
        self.inner.required.push(arg);
        self
    }

    pub fn optional(mut self, arg: PositionalArg) -> Self {
        self.inner.optional.push(arg);
        self
    }

    pub fn flag(mut self, flag: FlagSpec) -> Self {
        self.inner.flags.push(flag);
        self
    }

    pub fn rest(mut self, arg: PositionalArg) -> Self {
        self.inner.rest = Some(arg);
        self
    }

    pub fn build(self) -> Signature {
        self.inner
    }
}

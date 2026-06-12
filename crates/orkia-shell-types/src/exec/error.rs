// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! The execution error type.
//!
//! it carries the fields (`command`, `expected`, `got`, `upstream`) that a
//! typed error: never a panic, never a guess.

use thiserror::Error;

use crate::exec::typ::Type;

/// An error raised while binding arguments, type-checking, or running a
/// typed command pipeline.
#[derive(Debug, Error)]
pub enum ExecError {
    /// The product edge: an upstream's output type does not satisfy a
    /// downstream's input type. Generalizes the old `ParseError::AgentOnLeft`.
    #[error("type mismatch: `{command}` expected {expected:?} but `{upstream}` produced {got:?}")]
    TypeMismatch {
        /// The downstream command whose input slot was not satisfied.
        command: String,
        /// The type the downstream command requires.
        expected: Type,
        /// The type the upstream actually produced.
        got: Type,
        /// A label for the upstream producer (command name, `echo`, agent, ...).
        upstream: String,
    },

    /// A required positional argument was not supplied.
    #[error("`{command}`: missing required argument `{name}`")]
    MissingArg { command: String, name: String },

    /// An unrecognized flag was passed.
    #[error("`{command}`: unknown flag `{flag}`")]
    UnknownFlag { command: String, flag: String },

    /// An argument or literal could not be parsed/coerced.
    #[error("`{command}`: {message}")]
    BadArgs { command: String, message: String },

    /// A converter (or any command) was handed bytes it cannot interpret.
    #[error("`{command}`: bad value: {message}")]
    BadValue { command: String, message: String },

    /// A `ByteStream` reached a structured input slot with no explicit
    /// converter — the fail-closed refusal.
    #[error(
        "conversion refused: `{from}` produced bytes; an explicit converter (e.g. `from json`) is required"
    )]
    ConversionRefused { from: String },

    /// A command attempted an effect (FS / network / env) without the
    /// corresponding capability granted to this invocation. Fail-closed
    #[error("`{command}`: capability denied: {capability} ({detail})")]
    CapabilityDenied {
        command: String,
        capability: String,
        detail: String,
    },

    /// A failure inside a command's `run`.
    #[error("`{command}`: {message}")]
    Runtime { command: String, message: String },

    /// The pipeline was cancelled (e.g. early termination dropped the upstream).
    #[error("cancelled")]
    Cancelled,
}

impl ExecError {
    /// Invocation-shaped errors (the command never ran: bad flags, bad
    /// arity, type/conversion refusals) → 2; failures while running
    /// (denied capability, runtime error, cancellation) → 1.
    pub fn exit_code(&self) -> u8 {
        match self {
            ExecError::TypeMismatch { .. }
            | ExecError::MissingArg { .. }
            | ExecError::UnknownFlag { .. }
            | ExecError::BadArgs { .. }
            | ExecError::BadValue { .. }
            | ExecError::ConversionRefused { .. } => 2,
            ExecError::CapabilityDenied { .. }
            | ExecError::Runtime { .. }
            | ExecError::Cancelled => 1,
        }
    }
}

#[cfg(test)]
mod exit_code_tests {
    use super::ExecError;
    use crate::exec::typ::Type;

    #[test]
    fn invocation_errors_report_two() {
        let errors = [
            ExecError::TypeMismatch {
                command: "where".into(),
                expected: Type::Table,
                got: Type::String,
                upstream: "echo".into(),
            },
            ExecError::MissingArg {
                command: "log".into(),
                name: "target".into(),
            },
            ExecError::UnknownFlag {
                command: "ps".into(),
                flag: "--bogus".into(),
            },
            ExecError::BadArgs {
                command: "sort_by".into(),
                message: "x".into(),
            },
            ExecError::BadValue {
                command: "from json".into(),
                message: "x".into(),
            },
            ExecError::ConversionRefused { from: "cat".into() },
        ];
        for e in errors {
            assert_eq!(e.exit_code(), 2, "{e}");
        }
    }

    #[test]
    fn runtime_errors_report_one() {
        let errors = [
            ExecError::CapabilityDenied {
                command: "fetch".into(),
                capability: "net".into(),
                detail: "x".into(),
            },
            ExecError::Runtime {
                command: "ls".into(),
                message: "x".into(),
            },
            ExecError::Cancelled,
        ];
        for e in errors {
            assert_eq!(e.exit_code(), 1, "{e}");
        }
    }
}

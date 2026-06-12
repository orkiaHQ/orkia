// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Crate-local error type.

use thiserror::Error;

/// Which `*Assert` surface produced an `Assertion` error. Flow code
/// uses this to attach the right kind of diagnostic dump (PTY screen
/// for Output, journal tail for Journal, FS tree for File, etc.) to
/// the failure report without each call site duplicating boilerplate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssertKind {
    Output,
    Backend,
    File,
    Journal,
}

#[derive(Debug, Error)]
pub enum HarnessError {
    /// Returned by every skeleton method whose body is not wired yet.
    /// The `what` field names the surface so callers (and the
    /// `orkia-check` JSON report) can surface a precise diagnostic.
    #[error("not implemented: {what}")]
    NotImplemented { what: &'static str },

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    #[error("database: {0}")]
    Db(#[from] sqlx::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// An assertion failed. Carries:
    /// - `message`: one-line human-readable summary
    /// - `kind`: which `*Assert` surface raised it (so flow code can
    ///   pick the right state-dump strategy)
    /// - `state`: a pre-formatted dump of the relevant resource at the
    ///   moment of failure (FS tree, journal tail, PTY screen, etc.).
    ///   Captured at the assertion site so flow code does NOT have to
    ///   re-collect this information.
    #[error("assertion failed: {message}")]
    Assertion {
        message: String,
        kind: AssertKind,
        state: String,
    },

    #[error("timeout waiting for: {0}")]
    Timeout(String),

    #[error("infrastructure: {0}")]
    Infra(String),
}

impl HarnessError {
    /// Convenience constructor for assertion failures. Keeps the call
    /// sites in the `*Assert` builders short.
    pub fn assertion(
        message: impl Into<String>,
        kind: AssertKind,
        state: impl Into<String>,
    ) -> Self {
        Self::Assertion {
            message: message.into(),
            kind,
            state: state.into(),
        }
    }
}

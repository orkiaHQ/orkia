// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! Error type shared by the reasoning crates.

/// Errors raised while validating or (de)serializing reasoning records.
///
/// Pure-domain: no I/O variants live here — the store and client crates wrap
/// this with their own transport/storage errors.
#[derive(Debug, thiserror::Error)]
pub enum ReasoningError {
    /// A record failed to (de)serialize against the wire/storage contract.
    #[error("reasoning (de)serialization failed: {0}")]
    Serde(#[from] serde_json::Error),

    /// A record was structurally valid JSON but violated a domain invariant
    /// (e.g. a confidence outside `[0, 1]`).
    #[error("invalid reasoning record: {0}")]
    Invalid(String),
}

/// Convenience alias for fallible reasoning operations.
pub type Result<T> = std::result::Result<T, ReasoningError>;

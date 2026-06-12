// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! Client error type. Transport/auth/premium conditions are *not* errors — they
//! are reported as [`crate::SyncOutcome`] variants so the caller can keep the
//! local queue intact and surface state in `$reasoning status`. Only
//! unrecoverable construction/exhaustion failures bubble up here.

/// Errors raised by the reasoning HTTP client.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The HTTP client could not be constructed (TLS/runtime init).
    #[error("http init: {0}")]
    HttpInit(String),

    /// A request exhausted its retry budget against a retryable failure.
    #[error("request failed after {0} retries")]
    RetriesExhausted(u32),

    /// A response body could not be decoded into the expected shape.
    #[error("decode response: {0}")]
    Decode(String),
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.
#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! HTTP client to the cloud reasoning routes (`/v1/reasoning/*`).
//!
//! The shell captures locally (the SQLite store is the durable, tail-preserving
//! offline queue) and this crate is the thin transport that drains it: it pushes
//! dirty turns/signals to `POST /v1/reasoning/sync` (idempotent by
//! `client_event_id`) and pulls consolidated nodes/preferences back down. Bearer
//! auth + retry/back-off + status classification mirror
//! [`orkia-stream`](../orkia_stream)'s transport, the established pattern.
//!
//! There is **no** second JSONL queue here — that would duplicate the store's
//! dirty-row bookkeeping.

mod client;
mod error;

pub use client::{FetchScope, ReasoningClient, SyncBatch, SyncOutcome};
pub use error::ClientError;

/// Source of the bearer token sent on every request. The shell wires this to
/// `orkia_auth::AuthProvider::bearer`; tests supply a fixed token. Kept as a
/// crate-local trait so the client does not path-depend on the auth/keyring
/// stack (CLAUDE.md: one owner, minimal coupling).
pub trait BearerProvider: Send + Sync {
    /// The current bearer token, or `None` when logged out / expired.
    fn bearer(&self) -> Option<String>;
}

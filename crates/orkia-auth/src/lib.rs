// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Neutral authentication primitives for the Orkia shell.
//!
//! This crate provides two layers, both backend-agnostic:
//!
//!  * [`store`] — OS-level secret storage. `TokenStore<M>` trait
//!    plus `KeyringStore` (OS keychain) and `FileStore` (mode-0600
//!    file fallback). Generic over the metadata payload `M`; knows
//!    nothing about HTTP, OAuth, or any particular backend.
//!  * [`provider`] — `AuthProvider` trait + neutral [`SessionInfo`],
//!    [`AuthEvent`], [`AuthError`] types. The seam the shell consumes
//!    so it can render `$login`/`$logout`/`$whoami`/`$plan` without
//!    depending on a specific backend.
//!
//! There is no environment-injected session: every session is a real
//! backend login (signed JWT) persisted through [`store`]. Headless
//! callers point [`store::SESSION_FILE_ENV`] at a file the harness has
//! pre-populated with a genuine session. Proprietary backends implement
//! `AuthProvider` in their own crates and ship their own metadata shape.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod provider;
pub mod store;

pub use provider::{AuthError, AuthEvent, AuthEventSink, AuthProvider, SessionInfo};
pub use store::{
    FileStore, KeyringStore, SESSION_FILE_ENV, TokenStore, TokenStoreError, default_store,
    file_store_path,
};

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Magic-link login for the public `orkia` shell.
//!
//! Two-step protocol against the Orkia backend (mirrors `orkia-server`'s
//! `/auth/magic/{send,verify}`):
//!
//! 1. The shell POSTs the user's email to `/auth/magic/send`. The server
//!    emails a short-lived one-time code.
//! 2. The user pastes the code back; the shell POSTs it to
//!    `/auth/magic/verify` and receives a bearer JWT + account/workspace
//!    ids + plan.
//! 3. The credentials are persisted in the OS keychain via `orkia-auth`'s
//!    [`orkia_auth::TokenStore`] — the same source of truth the capability
//!    resolver and `$whoami`/`$plan` already read.
//!
//! This crate holds **no premium logic**: it only obtains and stores a
//! bearer. The plan claim it persists is what unlocks capabilities
//! downstream; the kernel daemon (the actual premium surface) is
//! provisioned separately, after login.
//!
//! The keychain entry name (`dev.orkia.cli`) and metadata schema are kept
//! byte-compatible with the proprietary distribution's login, so a session
//! created by either binary is readable by the other.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod http;
mod metadata;
mod provider;

pub use http::VerifyResponse;
pub use metadata::SessionMetadata;
pub use provider::MagicLinkAuthProvider;

use thiserror::Error;

/// Keychain service name. Shared verbatim with the proprietary login so
/// credentials interoperate between the public and proprietary binaries.
pub const KEYCHAIN_SERVICE: &str = "dev.orkia.cli";

/// Errors raised while running the magic-link flow.
#[derive(Debug, Error)]
pub enum MagicLoginError {
    #[error("network: {0}")]
    Http(String),
    #[error("server returned status {status}: {body}")]
    Server { status: u16, body: String },
    #[error("account has no workspace yet; finish onboarding at orkia.dev first")]
    NoWorkspace,
    #[error("no input on the terminal (login needs an interactive shell)")]
    NoInput,
    #[error("token store: {0}")]
    Storage(#[from] orkia_auth::TokenStoreError),
    #[error("runtime: {0}")]
    Runtime(String),
}

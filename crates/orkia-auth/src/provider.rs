// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `AuthProvider` — neutral seam between the shell and a concrete
//! authentication backend.
//!
//! The shell builds against this trait so it can render `$login`,
//! `$logout`, `$whoami`, and `$plan` without depending on any
//! particular backend's URL, JWT shape, or claim names. Concrete
//! implementations live elsewhere (e.g. the magic-link provider in
//! `orkia-magic-login` implements it for the Orkia backend, persisting a
//! real signed-JWT session through [`crate::store`]).

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::store::TokenStoreError;

/// Neutral snapshot of the currently authenticated session. Fields are
/// the minimum the shell renders in `$whoami`/`$plan`; backends are
/// free to discard any data they don't need to surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub display_name: String,
    pub email: String,
    pub plan: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Account (user) identifier from the session token, when the backend
    /// issued one. Used by Orkia Intelligence to scope the reasoning graph;
    /// `None` for env-injected or Forge-only sessions (intelligence stays
    /// inert without it — fail-closed).
    pub account_id: Option<String>,
    /// Active workspace identifier from the session token, when present.
    /// Same scoping role as [`Self::account_id`].
    pub workspace_id: Option<String>,
}

/// Lifecycle events emitted during an interactive `login` flow. The
/// shell observes these to render progress (browser opening, polling,
/// completion) without having to know any backend specifics.
#[derive(Debug, Clone)]
pub enum AuthEvent {
    OpeningBrowser { auth_url: String },
    AwaitingApproval,
    Polling,
    Completed { display_name: String },
    BrowserOpenFailed { reason: String },
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("network: {0}")]
    Network(String),
    #[error("login cancelled or timed out")]
    Cancelled,
    #[error("backend rejected the request: {0}")]
    Backend(String),
    #[error("token store: {0}")]
    Storage(#[from] TokenStoreError),
    #[error("not authenticated")]
    Unauthenticated,
    #[error("misconfigured: {0}")]
    Misconfigured(String),
}

/// Sink for [`AuthEvent`]s emitted during a login flow. Blanket impl
/// lets any `FnMut(AuthEvent) + Send + Sync` be passed.
pub trait AuthEventSink: Send + Sync {
    fn on_event(&mut self, ev: AuthEvent);
}

impl AuthEventSink for () {
    fn on_event(&mut self, _ev: AuthEvent) {}
}

impl<F> AuthEventSink for F
where
    F: FnMut(AuthEvent) + Send + Sync,
{
    fn on_event(&mut self, ev: AuthEvent) {
        self(ev)
    }
}

/// Neutral interface the shell consumes for all authentication
/// concerns. The shell holds an `Arc<dyn AuthProvider>` and never
/// names a concrete backend.
///
/// Methods are intentionally synchronous: backends that need async
/// work internally block on their own runtime, isolating the shell
/// from runtime choices.
pub trait AuthProvider: Send + Sync + 'static {
    /// Run the interactive login flow. The backend may open a
    /// browser, poll a server, or read from an environment variable —
    /// the shell does not care. Progress is reported via `sink`.
    fn login(&self, sink: &mut dyn AuthEventSink) -> Result<SessionInfo, AuthError>;

    /// Clear local credentials and best-effort revoke server-side.
    fn logout(&self) -> Result<(), AuthError>;

    /// Snapshot of the currently authenticated session, if any. No
    /// network round-trip — pure local read.
    fn current(&self) -> Option<SessionInfo>;

    /// Bearer token suitable for `Authorization: Bearer …` headers,
    /// when one is available. Consumers that only need to make
    /// authenticated requests use this without touching `SessionInfo`.
    fn bearer(&self) -> Option<String>;

    /// Adopt a server-issued bearer token without going through the
    /// interactive `login` flow. Used after `$invite accept` so the
    /// shell can carry the returned JWT into the new workspace
    /// without a manual `$logout`/`$login` round-trip.
    ///
    /// The default implementation returns `AuthError::Backend(...)`
    /// — backends that can't persist an externally-issued token
    /// (e.g. env-var providers) inherit this default. Concrete
    /// backends that own a token store override and persist.
    fn adopt_token(&self, _token: &str) -> Result<(), AuthError> {
        Err(AuthError::Backend(
            "this auth provider cannot adopt an externally-issued token; please re-login manually"
                .into(),
        ))
    }
}

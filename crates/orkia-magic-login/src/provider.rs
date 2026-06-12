// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! [`MagicLinkAuthProvider`] — an [`AuthProvider`] backed by the
//! magic-link flow plus a persisted session store.
//!
//! Every session is a REAL backend session: a signed JWT (carrying the
//! plan) obtained through the magic-link `send`/`verify` flow and saved
//! via the [`orkia_auth::TokenStore`] (keychain, or a file when
//! `ORKIA_SESSION_FILE` is set for headless harnesses). There is no
//! environment-injected bypass — the plan is never asserted by the
//! client; it comes from the signed token and is re-validated by the
//! kernel manifest/heartbeat.
//!
//! `login` itself is synchronous (the [`AuthProvider`] contract), but the
//! HTTP calls are async — we drive them on a private current-thread tokio
//! runtime. Callers run `login` under `spawn_blocking` (both `auth_cli`
//! and the `$login` builtin do), so blocking here never stalls the REPL.

use std::io::Write;

use chrono::Utc;
use orkia_auth::{
    AuthError, AuthEvent, AuthEventSink, AuthProvider, SessionInfo, TokenStore, default_store,
};

use crate::http::{self, VerifyResponse};
use crate::metadata::{self, SessionMetadata};
use crate::{KEYCHAIN_SERVICE, MagicLoginError};

/// Magic-link [`AuthProvider`] for the public shell.
pub struct MagicLinkAuthProvider {
    base_url: String,
    store: Box<dyn TokenStore<SessionMetadata>>,
}

impl MagicLinkAuthProvider {
    /// Build against `base_url` (e.g. `https://api.orkia.io`) using the
    /// platform keychain (file fallback where unsupported, or the
    /// `ORKIA_SESSION_FILE` path for headless harnesses).
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            store: default_store::<SessionMetadata>(KEYCHAIN_SERVICE),
        }
    }

    /// Build with an explicit store. Used by tests to swap in a temp
    /// [`orkia_auth::FileStore`] instead of touching the real keychain.
    pub fn with_store(
        base_url: impl Into<String>,
        store: Box<dyn TokenStore<SessionMetadata>>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            store,
        }
    }

    fn stored_session(&self) -> Option<(String, SessionMetadata)> {
        self.store.load().ok().flatten()
    }
}

impl AuthProvider for MagicLinkAuthProvider {
    fn login(&self, sink: &mut dyn AuthEventSink) -> Result<SessionInfo, AuthError> {
        let email = prompt("Email: ")?;
        run(http::send(&self.base_url, &email))??;
        sink.on_event(AuthEvent::AwaitingApproval);

        let nonce = prompt("Paste the code we just emailed you: ")?;
        sink.on_event(AuthEvent::Polling);
        let resp = run(http::verify(&self.base_url, &nonce))??;

        let meta = persist(self.store.as_ref(), &email, &resp)?;
        let info = metadata::to_session_info(&meta);
        sink.on_event(AuthEvent::Completed {
            display_name: info.display_name.clone(),
        });
        Ok(info)
    }

    fn logout(&self) -> Result<(), AuthError> {
        self.store.clear().map_err(AuthError::from)
    }

    fn current(&self) -> Option<SessionInfo> {
        let (_, meta) = self.stored_session()?;
        Some(metadata::to_session_info(&meta))
    }

    fn bearer(&self) -> Option<String> {
        self.stored_session().map(|(token, _)| token)
    }
}

impl From<MagicLoginError> for AuthError {
    fn from(e: MagicLoginError) -> Self {
        match e {
            MagicLoginError::Http(m) => AuthError::Network(m),
            MagicLoginError::Server { status, body } => {
                AuthError::Backend(format!("status {status}: {body}"))
            }
            MagicLoginError::NoWorkspace => AuthError::Backend(
                "account has no workspace yet; finish onboarding at orkia.dev first".into(),
            ),
            MagicLoginError::NoInput => AuthError::Cancelled,
            MagicLoginError::Storage(s) => AuthError::Storage(s),
            MagicLoginError::Runtime(m) => AuthError::Backend(m),
        }
    }
}

/// Drive an async future to completion on a private current-thread
/// runtime. Safe because `login` runs under `spawn_blocking`.
fn run<F: std::future::Future>(fut: F) -> Result<F::Output, MagicLoginError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| MagicLoginError::Runtime(e.to_string()))?;
    Ok(rt.block_on(fut))
}

/// Print `label` and read one trimmed, non-empty line from stdin.
fn prompt(label: &str) -> Result<String, MagicLoginError> {
    print!("{label}");
    std::io::stdout()
        .flush()
        .map_err(|e| MagicLoginError::Runtime(e.to_string()))?;
    let mut line = String::new();
    let read = std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| MagicLoginError::Runtime(e.to_string()))?;
    if read == 0 {
        return Err(MagicLoginError::NoInput);
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(MagicLoginError::NoInput);
    }
    Ok(trimmed.to_string())
}

/// Persist the verified credentials. Requires a workspace id — a session
/// without one can't scope reasoning or team features (fail-closed).
fn persist(
    store: &dyn TokenStore<SessionMetadata>,
    email: &str,
    resp: &VerifyResponse,
) -> Result<SessionMetadata, MagicLoginError> {
    if resp.workspace_id.is_none() {
        return Err(MagicLoginError::NoWorkspace);
    }
    let meta = SessionMetadata {
        account_id: resp.account_id.clone(),
        username: resp.username.clone().unwrap_or_else(|| email.to_string()),
        email: email.to_string(),
        plan: resp.plan.clone().unwrap_or_else(|| "free".into()),
        issued_at: Utc::now(),
        expires_at: None,
        workspace_id: resp.workspace_id.clone(),
    };
    store.save(&resp.token, &meta)?;
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_auth::FileStore;
    use tempfile::TempDir;

    fn provider(tmp: &TempDir) -> MagicLinkAuthProvider {
        let store = Box::new(FileStore::<SessionMetadata>::new(
            tmp.path().join("auth.toml"),
        ));
        MagicLinkAuthProvider::with_store("https://api.example.test", store)
    }

    fn verify_response() -> VerifyResponse {
        VerifyResponse {
            token: "bearer-xyz".into(),
            account_id: "acct-1".into(),
            workspace_id: Some("ws-1".into()),
            plan: Some("team".into()),
            username: Some("faye".into()),
        }
    }

    #[test]
    fn persist_requires_workspace() {
        let tmp = TempDir::new().unwrap();
        let store = FileStore::<SessionMetadata>::new(tmp.path().join("a.toml"));
        let mut resp = verify_response();
        resp.workspace_id = None;
        let err = persist(&store, "f@x.test", &resp).unwrap_err();
        assert!(matches!(err, MagicLoginError::NoWorkspace));
    }

    #[test]
    fn persist_then_read_back() {
        let tmp = TempDir::new().unwrap();
        let p = provider(&tmp);
        persist(p.store.as_ref(), "faye@x.test", &verify_response()).unwrap();

        let info = p.current().unwrap();
        assert_eq!(info.display_name, "faye");
        assert_eq!(info.plan, "team");
        assert_eq!(info.account_id.as_deref(), Some("acct-1"));
        assert_eq!(info.workspace_id.as_deref(), Some("ws-1"));
        assert_eq!(p.bearer().as_deref(), Some("bearer-xyz"));
    }

    #[test]
    fn logout_clears_store() {
        let tmp = TempDir::new().unwrap();
        let p = provider(&tmp);
        persist(p.store.as_ref(), "faye@x.test", &verify_response()).unwrap();
        p.logout().unwrap();
        assert!(p.current().is_none());
        assert!(p.bearer().is_none());
    }

    #[test]
    fn username_falls_back_to_email() {
        let tmp = TempDir::new().unwrap();
        let store = FileStore::<SessionMetadata>::new(tmp.path().join("a.toml"));
        let mut resp = verify_response();
        resp.username = None;
        let meta = persist(&store, "fallback@x.test", &resp).unwrap();
        assert_eq!(meta.username, "fallback@x.test");
    }
}

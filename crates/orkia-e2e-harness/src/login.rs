// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Real login against the compose backend, persisted to a file-backed
//! session store.
//!
//! The e2e harness no longer injects `ORKIA_TOKEN`/`ORKIA_PLAN` to fake a
//! session. Instead it logs in for real: `POST /auth/magic/send` (which
//! returns the one-time `nonce` under the backend's `test-fixtures`
//! feature), then `POST /auth/magic/verify` to exchange it for a signed
//! JWT carrying the plan. The verified session is written to
//! `$ORKIA_SESSION_FILE` via [`FileStore`] — the exact store the spawned
//! shell reads. The plan is whatever the backend resolved from the
//! fixture account's `organization.billing_plan`; the harness never
//! asserts it client-side.
//!
//! `boot.rs` calls this from inside the harness's async runtime, so the
//! blocking login runs on a dedicated OS thread (with its own single-thread
//! Tokio runtime) — Tokio forbids `block_on` on a thread already driving a
//! runtime, and the extra thread sidesteps that without making the whole
//! `boot.rs` setup path async.

use std::path::Path;

use chrono::Utc;
use orkia_auth::store::{FileStore, TokenStore};
use orkia_magic_login::SessionMetadata;
use serde::Deserialize;

/// Why a real login could not be completed. The harness degrades to "no
/// session" (Free) on any of these rather than aborting the whole run.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("build login runtime: {0}")]
    Runtime(String),
    #[error("send magic link: {0}")]
    Send(String),
    #[error("backend did not return a dev nonce (is `test-fixtures` enabled?)")]
    NoNonce,
    #[error("verify magic link: {0}")]
    Verify(String),
    #[error("persist session: {0}")]
    Persist(String),
}

#[derive(Deserialize)]
struct SendResponse {
    /// Present only under the backend's `test-fixtures` feature.
    nonce: Option<String>,
}

#[derive(Deserialize)]
struct VerifyResponse {
    token: String,
    account_id: String,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    username: Option<String>,
}

/// Log `email` in against `backend_url` and write the verified session to
/// `session_file`. Blocks the calling thread until the dedicated login
/// thread completes. On success the spawned shell, pointed at the same file
/// via `ORKIA_SESSION_FILE`, loads a genuine backend session.
pub fn login_to_session_file(
    backend_url: &str,
    email: &str,
    session_file: &Path,
) -> Result<(), LoginError> {
    // Run on a fresh OS thread that has no ambient runtime, so `block_on`
    // is legal even though the caller sits inside the harness's runtime.
    // `scope` lets the thread borrow the `&str` args without `'static`.
    let verified = std::thread::scope(|s| {
        s.spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| LoginError::Runtime(e.to_string()))?;
            rt.block_on(run_login(backend_url, email))
        })
        .join()
        .map_err(|_| LoginError::Runtime("login thread panicked".into()))?
    })?;
    persist(session_file, email, &verified)
}

async fn run_login(backend_url: &str, email: &str) -> Result<VerifyResponse, LoginError> {
    let base = backend_url.trim_end_matches('/');
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| LoginError::Send(e.to_string()))?;

    let send: SendResponse = client
        .post(format!("{base}/auth/magic/send"))
        .json(&serde_json::json!({ "email": email }))
        .send()
        .await
        .map_err(|e| LoginError::Send(e.to_string()))?
        .error_for_status()
        .map_err(|e| LoginError::Send(e.to_string()))?
        .json()
        .await
        .map_err(|e| LoginError::Send(e.to_string()))?;
    let nonce = send.nonce.ok_or(LoginError::NoNonce)?;

    let verified: VerifyResponse = client
        .post(format!("{base}/auth/magic/verify"))
        .json(&serde_json::json!({ "token": nonce }))
        .send()
        .await
        .map_err(|e| LoginError::Verify(e.to_string()))?
        .error_for_status()
        .map_err(|e| LoginError::Verify(e.to_string()))?
        .json()
        .await
        .map_err(|e| LoginError::Verify(e.to_string()))?;
    Ok(verified)
}

fn persist(session_file: &Path, email: &str, v: &VerifyResponse) -> Result<(), LoginError> {
    let meta = SessionMetadata {
        account_id: v.account_id.clone(),
        username: v.username.clone().unwrap_or_else(|| "e2e".into()),
        email: email.to_string(),
        plan: v.plan.clone().unwrap_or_else(|| "free".into()),
        issued_at: Utc::now(),
        expires_at: None,
        workspace_id: v.workspace_id.clone(),
    };
    FileStore::<SessionMetadata>::new(session_file.to_path_buf())
        .save(&v.token, &meta)
        .map_err(|e| LoginError::Persist(e.to_string()))
}

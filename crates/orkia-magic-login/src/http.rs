// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Pure HTTP plumbing for `/auth/magic/{send,verify}`. No keychain, no
//! TTY — the provider owns those and pairs these calls with persistence.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::MagicLoginError;

#[derive(Serialize)]
struct SendBody<'a> {
    email: &'a str,
}

#[derive(Serialize)]
struct VerifyBody<'a> {
    token: &'a str,
}

/// Response from `/auth/magic/verify`. `account_id`/`workspace_id` arrive
/// as JSON strings (UUIDs); kept as `String` to avoid a uuid dependency.
#[derive(Debug, Clone, Deserialize)]
pub struct VerifyResponse {
    pub token: String,
    pub account_id: String,
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Plan claim (`"solo-pro"`, `"team"`, …). When present the capability
    /// resolver sees the new plan immediately after persistence.
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
}

/// Request a one-time code be emailed to `email`. The server returns 200
/// even for unknown addresses (no account enumeration).
pub(crate) async fn send(base_url: &str, email: &str) -> Result<(), MagicLoginError> {
    let url = format!("{}/auth/magic/send", base_url.trim_end_matches('/'));
    let resp = client()?
        .post(&url)
        .json(&SendBody { email })
        .send()
        .await
        .map_err(|e| MagicLoginError::Http(e.to_string()))?;
    error_for_status(resp).await?;
    Ok(())
}

/// Exchange the one-time code for a bearer JWT + profile.
pub(crate) async fn verify(base_url: &str, nonce: &str) -> Result<VerifyResponse, MagicLoginError> {
    let url = format!("{}/auth/magic/verify", base_url.trim_end_matches('/'));
    let resp = client()?
        .post(&url)
        .json(&VerifyBody { token: nonce })
        .send()
        .await
        .map_err(|e| MagicLoginError::Http(e.to_string()))?;
    let ok = error_for_status(resp).await?;
    ok.json::<VerifyResponse>()
        .await
        .map_err(|e| MagicLoginError::Http(format!("parse verify response: {e}")))
}

/// Surface non-2xx as [`MagicLoginError::Server`] with the body, else pass
/// the response through.
async fn error_for_status(resp: reqwest::Response) -> Result<reqwest::Response, MagicLoginError> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Err(MagicLoginError::Server { status, body })
}

fn client() -> Result<reqwest::Client, MagicLoginError> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("orkia/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| MagicLoginError::Http(e.to_string()))
}

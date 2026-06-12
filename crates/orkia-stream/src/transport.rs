// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! HTTP transport for `/api/sync/push`.
//!
//! One NDJSON request per flushable batch. Retries on transport
//! failures and 5xx with exponential back-off; 401 → AuthExpired;

use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::AuthContext;
use crate::batch::Batch;
use crate::errors::StreamError;

const PUSH_PATH: &str = "/api/sync/push";
const MAX_RETRIES: u32 = 6;
const BACKOFF_STEPS: &[u64] = &[1, 2, 4, 8, 16, 30];

#[derive(Debug, Clone, Deserialize)]
struct PushResponseShape {
    accepted: usize,
    #[serde(default)]
    errors: usize,
    #[serde(default)]
    results: Vec<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub enum PushOutcome {
    Accepted {
        accepted: usize,
    },
    /// Server returned a non-retryable error (400/403). The whole batch
    /// is dropped; the cursor still advances.
    Dropped,
    /// Auth token expired (401) and refresh failed. Caller pauses.
    AuthExpired,
}

#[derive(Clone)]
pub struct HttpClient {
    client: Client,
    base_url: String,
    auth: AuthContext,
}

impl HttpClient {
    pub fn new(base_url: String, auth: AuthContext) -> Result<Self, StreamError> {
        let client = Client::builder()
            .user_agent(concat!("orkia-stream/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| StreamError::HttpInit(e.to_string()))?;
        Ok(Self {
            client,
            base_url,
            auth,
        })
    }

    pub async fn push_batch(&self, batch: &Batch) -> Result<PushOutcome, StreamError> {
        if batch.lines().is_empty() {
            // Cursor-only advance (every line in this batch was dropped
            // by the scope gate). Nothing to send.
            return Ok(PushOutcome::Accepted { accepted: 0 });
        }
        let body = batch.to_ndjson();
        let idempotency_key = Uuid::now_v7().to_string();
        let url = format!("{}{}", self.base_url, PUSH_PATH);

        let mut attempt: u32 = 0;
        loop {
            let bearer = self.auth.bearer();
            let Some(bearer) = bearer else {
                return Ok(PushOutcome::AuthExpired);
            };
            let req = self
                .client
                .post(&url)
                .bearer_auth(&bearer)
                .header("Content-Type", "application/x-ndjson")
                .header("X-Idempotency-Key", &idempotency_key)
                .body(body.clone());
            let outcome = match req.send().await {
                Ok(resp) => classify(resp).await,
                Err(e) => {
                    tracing::warn!(error = %e, attempt, "orkia-stream: push transport error");
                    AttemptOutcome::Retry
                }
            };
            match outcome {
                AttemptOutcome::Ok(accepted) => return Ok(PushOutcome::Accepted { accepted }),
                AttemptOutcome::Dropped => return Ok(PushOutcome::Dropped),
                AttemptOutcome::AuthExpired => {
                    if self.auth.try_refresh() {
                        attempt = attempt.saturating_add(1);
                        if attempt >= MAX_RETRIES {
                            return Ok(PushOutcome::AuthExpired);
                        }
                        continue;
                    }
                    return Ok(PushOutcome::AuthExpired);
                }
                AttemptOutcome::Retry => {
                    attempt = attempt.saturating_add(1);
                    if attempt >= MAX_RETRIES {
                        return Err(StreamError::HttpInit(format!(
                            "push failed after {MAX_RETRIES} retries",
                        )));
                    }
                    let secs = BACKOFF_STEPS
                        .get(attempt as usize - 1)
                        .copied()
                        .unwrap_or(30);
                    tokio::time::sleep(Duration::from_secs(secs)).await;
                }
                AttemptOutcome::RetryAfter(secs) => {
                    attempt = attempt.saturating_add(1);
                    if attempt >= MAX_RETRIES {
                        return Err(StreamError::HttpInit("push retry-after limit".into()));
                    }
                    tokio::time::sleep(Duration::from_secs(secs)).await;
                }
            }
        }
    }
}

enum AttemptOutcome {
    Ok(usize),
    Dropped,
    AuthExpired,
    Retry,
    RetryAfter(u64),
}

async fn classify(resp: reqwest::Response) -> AttemptOutcome {
    let status = resp.status();
    if status.is_success() {
        match resp.json::<PushResponseShape>().await {
            Ok(p) => {
                if p.errors > 0 {
                    tracing::warn!(
                        errors = p.errors,
                        results = p.results.len(),
                        "orkia-stream: server reported per-line errors; cursor still advances",
                    );
                }
                AttemptOutcome::Ok(p.accepted)
            }
            Err(_) => AttemptOutcome::Ok(0),
        }
    } else if status == StatusCode::UNAUTHORIZED {
        AttemptOutcome::AuthExpired
    } else if status == StatusCode::BAD_REQUEST || status == StatusCode::FORBIDDEN {
        let body = resp.text().await.unwrap_or_default();
        tracing::error!(status = %status, body = %body, "orkia-stream: server rejected batch; dropping");
        AttemptOutcome::Dropped
    } else if status == StatusCode::SERVICE_UNAVAILABLE {
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(5);
        AttemptOutcome::RetryAfter(retry_after)
    } else if status.is_server_error() {
        AttemptOutcome::Retry
    } else {
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!(status = %status, body = %body, "orkia-stream: unexpected status; dropping batch");
        AttemptOutcome::Dropped
    }
}

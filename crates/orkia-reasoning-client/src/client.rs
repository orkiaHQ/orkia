// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! The HTTP client: one POST (`sync`) drains dirty rows, two GETs pull
//! consolidated nodes/preferences. Retry/back-off + status classification
//! mirror `orkia-stream`'s transport.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use orkia_reasoning_core::dto::{KnowledgeNode, PreferenceDto, SignalDto, TurnDto};

use crate::BearerProvider;
use crate::error::ClientError;

const SYNC_PATH: &str = "/v1/reasoning/sync";
const MAX_RETRIES: u32 = 6;
const BACKOFF_STEPS: &[u64] = &[1, 2, 4, 8, 16, 30];

/// A batch of dirty rows to push. Turns and signals share one idempotent
/// request so a replay (same `client_event_id`s) dedupes server-side.
#[derive(Debug, Default, Serialize)]
pub struct SyncBatch {
    pub turns: Vec<TurnDto>,
    pub signals: Vec<SignalDto>,
}

impl SyncBatch {
    /// True when there is nothing to push (skip the request entirely).
    pub fn is_empty(&self) -> bool {
        self.turns.is_empty() && self.signals.is_empty()
    }
}

/// The outcome of a `sync_batch` call. Transport/auth/premium conditions are
/// outcomes, not errors: the caller keeps the local queue intact and reflects
/// state in `$reasoning status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Server accepted the batch; `accepted` rows were ingested (idempotent).
    Accepted { accepted: usize },
    /// 403 premium_required — caller sets the sticky flag and stops syncing
    /// until the next login (audit risk #4).
    PremiumRequired,
    /// 401 — token expired and refresh failed. Caller pauses, retries on login.
    AuthExpired,
    /// 400 — the batch was malformed and rejected; drop it so it can't wedge
    /// the queue (the rows stay marked dirty only if the caller chooses).
    Dropped,
}

/// Optional filters for the pull endpoints. All `None` ⇒ unscoped (whole
/// workspace). `since` enables incremental pulls.
#[derive(Debug, Default, Clone)]
pub struct FetchScope {
    pub since: Option<DateTime<Utc>>,
    pub project_id: Option<Uuid>,
    pub rfc_id: Option<String>,
}

impl FetchScope {
    /// Parse `base` and append the set scope fields as (percent-encoded) query
    /// params, returning the full URL.
    fn apply_to(&self, base: &str) -> Result<url::Url, ClientError> {
        let mut url = url::Url::parse(base).map_err(|e| ClientError::HttpInit(e.to_string()))?;
        // Only touch the query when at least one field is set — `query_pairs_mut`
        // stamps a bare `?` otherwise.
        if self.since.is_some() || self.project_id.is_some() || self.rfc_id.is_some() {
            let mut q = url.query_pairs_mut();
            if let Some(since) = self.since {
                q.append_pair("since", &since.to_rfc3339());
            }
            if let Some(project_id) = self.project_id {
                q.append_pair("project_id", &project_id.to_string());
            }
            if let Some(rfc_id) = &self.rfc_id {
                q.append_pair("rfc_id", rfc_id);
            }
        }
        Ok(url)
    }
}

#[derive(Deserialize)]
struct SyncResponseShape {
    accepted: usize,
    #[serde(default)]
    errors: usize,
}

#[derive(Default, Deserialize)]
struct NodesResponseShape {
    #[serde(default)]
    nodes: Vec<KnowledgeNode>,
}

#[derive(Default, Deserialize)]
struct PreferencesResponseShape {
    #[serde(default)]
    preferences: Vec<PreferenceDto>,
}

/// HTTP client to the cloud reasoning routes. Cheap to clone (shares the
/// connection pool and bearer source).
#[derive(Clone)]
pub struct ReasoningClient {
    client: Client,
    base_url: String,
    bearer: Arc<dyn BearerProvider>,
}

impl ReasoningClient {
    /// Construct a client posting to `base_url` (no trailing slash), drawing the
    /// bearer from `bearer` on every attempt so refreshed tokens are picked up.
    pub fn new(base_url: String, bearer: Arc<dyn BearerProvider>) -> Result<Self, ClientError> {
        let client = Client::builder()
            .user_agent(concat!(
                "orkia-reasoning-client/",
                env!("CARGO_PKG_VERSION")
            ))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| ClientError::HttpInit(e.to_string()))?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            bearer,
        })
    }

    /// Push dirty turns/signals. Idempotent: re-pushing the same batch dedupes
    /// server-side by `client_event_id`.
    pub async fn sync_batch(&self, batch: &SyncBatch) -> Result<SyncOutcome, ClientError> {
        if batch.is_empty() {
            return Ok(SyncOutcome::Accepted { accepted: 0 });
        }
        let body = serde_json::to_vec(batch).map_err(|e| ClientError::Decode(e.to_string()))?;
        let url = format!("{}{}", self.base_url, SYNC_PATH);
        let idem = Uuid::now_v7().to_string();

        let mut attempt: u32 = 0;
        loop {
            let Some(bearer) = self.bearer.bearer() else {
                return Ok(SyncOutcome::AuthExpired);
            };
            let req = self
                .client
                .post(&url)
                .bearer_auth(&bearer)
                .header("Content-Type", "application/json")
                .header("X-Idempotency-Key", &idem)
                .body(body.clone());
            let outcome = match req.send().await {
                Ok(resp) => classify_sync(resp).await,
                Err(e) => {
                    tracing::warn!(error = %e, attempt, "reasoning-client: sync transport error");
                    AttemptOutcome::Retry
                }
            };
            match outcome {
                AttemptOutcome::Done(o) => return Ok(o),
                AttemptOutcome::Retry => {
                    attempt = attempt.saturating_add(1);
                    if attempt >= MAX_RETRIES {
                        return Err(ClientError::RetriesExhausted(MAX_RETRIES));
                    }
                    sleep_backoff(attempt).await;
                }
                AttemptOutcome::RetryAfter(secs) => {
                    attempt = attempt.saturating_add(1);
                    if attempt >= MAX_RETRIES {
                        return Err(ClientError::RetriesExhausted(MAX_RETRIES));
                    }
                    tokio::time::sleep(Duration::from_secs(secs)).await;
                }
            }
        }
    }

    /// Pull consolidated knowledge nodes for a workspace, optionally scoped.
    pub async fn fetch_nodes(
        &self,
        workspace_id: Uuid,
        scope: &FetchScope,
    ) -> Result<Vec<KnowledgeNode>, ClientError> {
        let path = format!("/v1/reasoning/nodes/{workspace_id}");
        let shape: NodesResponseShape = self.get_scoped(&path, scope).await?;
        Ok(shape.nodes)
    }

    /// Pull effective preferences for a workspace, optionally scoped.
    pub async fn fetch_preferences(
        &self,
        workspace_id: Uuid,
        scope: &FetchScope,
    ) -> Result<Vec<PreferenceDto>, ClientError> {
        let path = format!("/v1/reasoning/preferences/{workspace_id}");
        let shape: PreferencesResponseShape = self.get_scoped(&path, scope).await?;
        Ok(shape.preferences)
    }

    /// Shared GET-with-retry for the two pull endpoints. Decodes the JSON body
    /// on success; retries transport/5xx; non-success non-retryable ⇒ empty
    /// (pulls are best-effort — the local store still serves reads).
    async fn get_scoped<T: for<'de> Deserialize<'de> + Default>(
        &self,
        path: &str,
        scope: &FetchScope,
    ) -> Result<T, ClientError> {
        let url = scope.apply_to(&format!("{}{}", self.base_url, path))?;
        let mut attempt: u32 = 0;
        loop {
            let Some(bearer) = self.bearer.bearer() else {
                return Ok(T::default());
            };
            let req = self.client.get(url.clone()).bearer_auth(&bearer);
            match req.send().await {
                Ok(resp) if resp.status().is_success() => {
                    return resp
                        .json::<T>()
                        .await
                        .map_err(|e| ClientError::Decode(e.to_string()));
                }
                Ok(resp) if resp.status().is_server_error() => {
                    tracing::warn!(status = %resp.status(), "reasoning-client: pull 5xx; retrying");
                }
                Ok(resp) => {
                    tracing::warn!(status = %resp.status(), "reasoning-client: pull non-success; empty");
                    return Ok(T::default());
                }
                Err(e) => {
                    tracing::warn!(error = %e, attempt, "reasoning-client: pull transport error");
                }
            }
            attempt = attempt.saturating_add(1);
            if attempt >= MAX_RETRIES {
                return Err(ClientError::RetriesExhausted(MAX_RETRIES));
            }
            sleep_backoff(attempt).await;
        }
    }
}

/// Internal per-attempt result for the sync loop.
enum AttemptOutcome {
    Done(SyncOutcome),
    Retry,
    RetryAfter(u64),
}

async fn sleep_backoff(attempt: u32) {
    let secs = BACKOFF_STEPS
        .get(attempt as usize - 1)
        .copied()
        .unwrap_or(30);
    tokio::time::sleep(Duration::from_secs(secs)).await;
}

async fn classify_sync(resp: reqwest::Response) -> AttemptOutcome {
    let status = resp.status();
    if status.is_success() {
        match resp.json::<SyncResponseShape>().await {
            Ok(p) => {
                if p.errors > 0 {
                    tracing::warn!(errors = p.errors, "reasoning-client: server per-row errors");
                }
                AttemptOutcome::Done(SyncOutcome::Accepted {
                    accepted: p.accepted,
                })
            }
            // Accepted but unparseable body — treat as accepted-zero rather than
            // re-pushing (idempotency makes a re-push safe, but pointless).
            Err(_) => AttemptOutcome::Done(SyncOutcome::Accepted { accepted: 0 }),
        }
    } else if status == StatusCode::UNAUTHORIZED {
        AttemptOutcome::Done(SyncOutcome::AuthExpired)
    } else if status == StatusCode::FORBIDDEN {
        AttemptOutcome::Done(SyncOutcome::PremiumRequired)
    } else if status == StatusCode::BAD_REQUEST {
        let body = resp.text().await.unwrap_or_default();
        tracing::error!(body = %body, "reasoning-client: server rejected batch (400); dropping");
        AttemptOutcome::Done(SyncOutcome::Dropped)
    } else if status == StatusCode::SERVICE_UNAVAILABLE {
        let secs = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(5);
        AttemptOutcome::RetryAfter(secs)
    } else if status.is_server_error() {
        AttemptOutcome::Retry
    } else {
        tracing::warn!(status = %status, "reasoning-client: unexpected status; dropping batch");
        AttemptOutcome::Done(SyncOutcome::Dropped)
    }
}

#[cfg(test)]
mod tests;

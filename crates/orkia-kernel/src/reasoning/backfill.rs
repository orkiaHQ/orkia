// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! One-shot cold-path drain for `orkia reasoning backfill`. Builds a
//! throwaway [`SyncWorker`] against an already-staged store and runs a single
//! push+pull, then returns — no interval loop, no shared lifecycle.
//!
//! Staging (parsing historical transcripts into turns via the real
//! [`ReasoningConsumer`]) is the caller's job; this only flushes the dirty rows
//! that staging wrote up to the cloud, exactly as a live sync tick would. The
//! worker owns its own store connection (one owner, #2) and is dropped when the
//! drain returns.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::Notify;

use orkia_auth::AuthProvider;
use orkia_reasoning_client::{BearerProvider, ReasoningClient};
use orkia_reasoning_core::PreferenceCache;

use super::{CaptureScope, SYNC_INTERVAL, SyncConfig, SyncWorker};
use crate::intelligence::KernelError;

/// Adapts the auth provider to the client's bearer source, so each request
/// draws a fresh token. Mirrors the live `Intelligence` handle's wrapper; kept
/// local so backfill stays standalone (no dependency on the REPL boot path).
struct AuthBearer(Arc<dyn AuthProvider>);
impl BearerProvider for AuthBearer {
    fn bearer(&self) -> Option<String> {
        self.0.bearer()
    }
}

/// Inputs for [`backfill_sync`]. A config struct keeps the entry within the
/// four-argument limit.
pub struct BackfillSyncConfig {
    /// The local store the staging pass already populated with dirty turns.
    pub store_path: PathBuf,
    /// Workspace/account the turns were captured under (drives the push scope).
    pub scope: CaptureScope,
    /// Cloud base URL. `resolve_backend_url` enforces `https://` upstream.
    pub backend_url: String,
    /// Auth provider supplying the bearer for each request.
    pub auth: Arc<dyn AuthProvider>,
}

/// Flush the store's dirty turns/signals to the cloud once (push), then pull
/// any consolidated nodes/preferences back (pull). Returns when both complete.
///
/// Fail-closed: a client-init failure is a hard error. A network failure
/// *inside* push/pull is not — those rows simply stay `dirty` for the next
/// drain (the worker logs and moves on), so a partial cloud outage degrades to
/// "try again" rather than data loss.
pub async fn backfill_sync(cfg: BackfillSyncConfig) -> Result<(), KernelError> {
    let bearer = Arc::new(AuthBearer(cfg.auth));
    let client = ReasoningClient::new(cfg.backend_url, bearer)
        .map_err(|e| KernelError::Sync(e.to_string()))?;
    let worker = SyncWorker::open(SyncConfig {
        store_path: cfg.store_path,
        workspace_id: cfg.scope.workspace_id,
        account_id: cfg.scope.account_id,
        client,
        prefs: Arc::new(PreferenceCache::new()),
        interval: SYNC_INTERVAL,
        premium_denied: Arc::new(AtomicBool::new(false)),
        trigger: Arc::new(Notify::new()),
        audit: None,
    })?;
    worker.sync_once().await;
    Ok(())
}

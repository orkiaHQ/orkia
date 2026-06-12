// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! The cold-path sync worker. A background task that, on an interval,
//! drains the local store's dirty turns/signals up to the cloud and pulls
//! consolidated nodes/preferences back down. It owns its **own** SQLite
//! connection to the same file the consumer writes (one owner per
//! connection; WAL lets them run concurrently — see `ReasoningStore::init`).
//!
//! Transport/auth/premium states are outcomes, not errors: on `PremiumRequired`
//! it sets a sticky flag and stops syncing until the next login;
//! on `AuthExpired`/transport failure it leaves rows dirty for the next tick.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use uuid::Uuid;

use orkia_reasoning_client::{FetchScope, ReasoningClient, SyncBatch, SyncOutcome};
use orkia_reasoning_core::PreferenceCache;
use orkia_reasoning_store::{NodeInsert, PrefUpsert, ReasoningStore, StoreError};

use super::audit::ReasoningAudit;

/// Rows pushed/pulled per tick — bounds request size and store work.
const BATCH: usize = 200;

/// How often the worker syncs. Long enough to coalesce a tool-call storm into
/// one request, short enough that `$reasoning status` drains visibly.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(30);

/// How often local GC runs (much rarer than sync). Trims consolidated turn text
/// and prunes long-superseded nodes so the on-disk store stays bounded.
const GC_INTERVAL: Duration = Duration::from_secs(3600);

/// Consolidated turns keep their hash + links forever, but their heavy text
/// (summary/thinking) is dropped after this age — the cloud is the system of
/// record for old reasoning, the local store is a working cache.
const TURN_TEXT_MAX_AGE: chrono::Duration = chrono::Duration::days(30);

/// Superseded nodes are deleted this long after being replaced.
const SUPERSEDED_RETENTION: chrono::Duration = chrono::Duration::days(7);

/// Everything the worker needs to run. The store is opened from `store_path`.
pub struct SyncConfig {
    pub store_path: PathBuf,
    pub workspace_id: Uuid,
    pub account_id: Uuid,
    pub client: ReasoningClient,
    pub prefs: Arc<PreferenceCache>,
    pub interval: Duration,
    /// Sticky premium-denied flag, shared with the Intelligence handle so
    /// `$reasoning status` and the next login can read/reset it.
    pub premium_denied: Arc<AtomicBool>,
    /// Manual-sync trigger, shared with the Intelligence handle. `$reasoning
    /// sync` notifies it to wake the worker before its next interval tick
    /// (the REPL sends a message, never touches the store).
    pub trigger: Arc<Notify>,
    /// Optional SEAL audit sink. When set, each pull that consolidates nodes
    /// emits a `reasoning.nodes_consolidated` workspace event. `None` in
    /// offline/test setups.
    pub audit: Option<Arc<dyn ReasoningAudit>>,
}

/// The sync worker. Single-owner of its store connection and HTTP client.
pub struct SyncWorker {
    store: ReasoningStore,
    workspace_id: Uuid,
    account_id: Uuid,
    client: ReasoningClient,
    prefs: Arc<PreferenceCache>,
    interval: Duration,
    premium_denied: Arc<AtomicBool>,
    trigger: Arc<Notify>,
    audit: Option<Arc<dyn ReasoningAudit>>,
    /// When GC last ran. `None` until the first GC tick after boot.
    last_gc: Option<Instant>,
}

impl SyncWorker {
    /// Open the worker's own connection to the store file. Fails only if the
    /// store can't be opened (the caller then skips sync, keeping the hot path).
    pub fn open(cfg: SyncConfig) -> Result<Self, StoreError> {
        let store = ReasoningStore::open(&cfg.store_path)?;
        Ok(Self {
            store,
            workspace_id: cfg.workspace_id,
            account_id: cfg.account_id,
            client: cfg.client,
            prefs: cfg.prefs,
            interval: cfg.interval,
            premium_denied: cfg.premium_denied,
            trigger: cfg.trigger,
            audit: cfg.audit,
            last_gc: None,
        })
    }

    /// Run until the task is aborted (on `$logout`/shutdown). Each tick pushes
    /// then pulls; a sticky premium denial parks the loop (idle, no requests).
    ///
    /// `&mut self` (not `&self`) on the per-tick methods is load-bearing for
    /// `tokio::spawn`: the worker owns a `rusqlite::Connection`, which is `Send`
    /// but not `Sync`, so a `&self` borrow held across a network `.await` would
    /// make the future non-`Send`. `&mut SyncWorker: Send` (since `SyncWorker:
    /// Send`), so an exclusive borrow across the await is fine.
    pub async fn run(mut self) {
        loop {
            // Wake on whichever comes first: the interval tick, or a manual
            // `$reasoning sync` notification. A sticky premium denial still
            // parks the loop below — it only clears on re-login (boot), so a
            // manual sync while denied is a deliberate no-op (fail-closed, #8).
            tokio::select! {
                _ = tokio::time::sleep(self.interval) => {}
                _ = self.trigger.notified() => {}
            }
            if self.premium_denied.load(Ordering::Relaxed) {
                continue;
            }
            self.push().await;
            self.pull().await;
            self.maybe_gc();
        }
    }

    /// One push then pull, then return — the one-shot drain backfill uses
    /// (`orkia reasoning backfill`) instead of the interval `run` loop. A
    /// network failure inside push/pull leaves rows dirty for a later attempt
    /// (logged, never fatal, #8); GC is skipped (backfill is not long-lived).
    pub async fn sync_once(mut self) {
        self.push().await;
        self.pull().await;
    }

    /// Run local GC if `GC_INTERVAL` has elapsed since the last run (or this is
    /// the first tick). Trims old consolidated turn text and prunes
    /// long-superseded nodes. Errors are logged, never fatal. Runs on
    /// the worker's own connection — no extra owner.
    fn maybe_gc(&mut self) {
        let due = self.last_gc.is_none_or(|t| t.elapsed() >= GC_INTERVAL);
        if !due {
            return;
        }
        self.last_gc = Some(Instant::now());
        match self.store.gc_consolidated_turns(TURN_TEXT_MAX_AGE) {
            Ok(n) if n > 0 => tracing::debug!(trimmed = n, "sync: gc trimmed turn text"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "sync: gc of turns failed"),
        }
        match self.store.prune_superseded(SUPERSEDED_RETENTION) {
            Ok(n) if n > 0 => tracing::debug!(pruned = n, "sync: gc pruned superseded nodes"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "sync: prune of nodes failed"),
        }
    }

    /// Push dirty turns + signals once. Store errors are logged, not fatal.
    pub(crate) async fn push(&mut self) {
        let (turns, signals) = match self.collect_dirty() {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "sync: reading dirty rows failed");
                return;
            }
        };
        if turns.is_empty() && signals.is_empty() {
            return;
        }
        let turn_ids: Vec<Uuid> = turns.iter().map(|t| t.client_event_id).collect();
        let signal_ids: Vec<Uuid> = signals.iter().map(|s| s.client_event_id).collect();
        let batch = SyncBatch { turns, signals };
        // Clone the (Send+Sync) client into a local so the non-`Sync` store
        // connection in `self` is never borrowed across the network await.
        let client = self.client.clone();
        match client.sync_batch(&batch).await {
            // Accepted (idempotent) or a 400 the server will never accept: clear
            // the rows either way so a poison batch can't wedge the queue.
            Ok(SyncOutcome::Accepted { accepted }) => {
                tracing::debug!(accepted, "sync: batch accepted");
                self.clear_synced(&turn_ids, &signal_ids);
            }
            Ok(SyncOutcome::Dropped) => {
                tracing::error!(
                    turns = turn_ids.len(),
                    "sync: server dropped batch (400); clearing"
                );
                self.clear_synced(&turn_ids, &signal_ids);
            }
            Ok(SyncOutcome::PremiumRequired) => {
                tracing::info!("sync: premium required — parking sync until next login");
                self.premium_denied.store(true, Ordering::Relaxed);
            }
            // Leave rows dirty; the next tick (or next login) retries.
            Ok(SyncOutcome::AuthExpired) => tracing::info!("sync: auth expired; rows kept dirty"),
            Err(e) => tracing::warn!(error = %e, "sync: push failed; rows kept dirty"),
        }
    }

    /// Pull consolidated nodes + preferences once, writing them to the store and
    /// refreshing the in-memory preference cache. Best-effort.
    pub(crate) async fn pull(&mut self) {
        let scope = FetchScope::default();
        // Clone the client locally so the non-`Sync` store in `self` is not
        // borrowed across the awaits (store writes happen after each await).
        let client = self.client.clone();
        match client.fetch_nodes(self.workspace_id, &scope).await {
            Ok(nodes) => {
                let stored = self.store_nodes(&nodes);
                self.audit_consolidated(&stored);
            }
            Err(e) => tracing::warn!(error = %e, "sync: node pull failed"),
        }
        match client.fetch_preferences(self.workspace_id, &scope).await {
            Ok(prefs) if !prefs.is_empty() => self.store_prefs(prefs),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "sync: preference pull failed"),
        }
    }

    fn collect_dirty(
        &self,
    ) -> Result<
        (
            Vec<orkia_reasoning_core::dto::TurnDto>,
            Vec<orkia_reasoning_core::dto::SignalDto>,
        ),
        StoreError,
    > {
        Ok((
            self.store.dirty_turn_dtos(BATCH)?,
            self.store.dirty_signals(BATCH)?,
        ))
    }

    fn clear_synced(&self, turn_ids: &[Uuid], signal_ids: &[Uuid]) {
        if let Err(e) = self.store.mark_turns_synced(turn_ids) {
            tracing::warn!(error = %e, "sync: marking turns synced failed");
        }
        if let Err(e) = self.store.mark_signals_synced(signal_ids) {
            tracing::warn!(error = %e, "sync: marking signals synced failed");
        }
    }

    /// Upsert pulled nodes, returning the ids that landed (for the audit seal).
    fn store_nodes(&self, nodes: &[orkia_reasoning_core::dto::KnowledgeNode]) -> Vec<Uuid> {
        let mut stored = Vec::with_capacity(nodes.len());
        for node in nodes {
            let insert = NodeInsert {
                node,
                details: None,
                domain: None,
                context_block: None,
                source_turn_id: None,
                source_session_id: None,
                seal_id: None,
            };
            match self.store.upsert_node(&insert) {
                Ok(()) => stored.push(node.id),
                Err(e) => tracing::warn!(error = %e, "sync: upsert node failed"),
            }
        }
        stored
    }

    /// Emit the workspace-chain audit record for a batch of consolidated nodes.
    /// The chain becomes the authoritative provenance log for cloud-added
    /// knowledge (the node ids are enumerated in the sealed payload).
    fn audit_consolidated(&self, node_ids: &[Uuid]) {
        if node_ids.is_empty() {
            return;
        }
        if let Some(audit) = self.audit.as_ref() {
            audit.nodes_consolidated(node_ids, None);
        }
    }

    fn store_prefs(&self, prefs: Vec<orkia_reasoning_core::dto::PreferenceDto>) {
        for pref in &prefs {
            let upsert = PrefUpsert {
                workspace_id: self.workspace_id,
                account_id: self.account_id,
                pref: pref.clone(),
                scope_id: None,
            };
            if let Err(e) = self.store.upsert_preference(&upsert) {
                tracing::warn!(error = %e, "sync: upsert preference failed");
            }
        }
        // Refresh the lock-free cache the enrich path reads at spawn.
        self.prefs.put(self.workspace_id, prefs);
    }
}

#[cfg(test)]
#[path = "sync_tests.rs"]
mod sync_tests;

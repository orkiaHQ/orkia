// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! The central Intelligence handle the REPL boots and holds. It owns the
//! gate, the classification + model facades, the lock-free preference cache
//! used at enrich time, and the lifecycle of the reasoning consumer task.
//!
//! Boot is gated: when the gate is closed (anonymous or free plan), `boot`
//! constructs nothing — no store dir, no task — and returns `false`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{Notify, broadcast};
use tokio::task::JoinHandle;
use uuid::Uuid;

use orkia_auth::AuthProvider;
use orkia_reasoning_client::ReasoningClient;
use orkia_reasoning_core::{PreferenceCache, ReasoningContext, append_knowledge_protocol};
use orkia_reasoning_store::ReasoningStore;
use orkia_shell_types::{KernelRpc, journal::JournalEnvelope};

use crate::classify::Classifier;
use crate::gate::{Gate, GateState, Identity};
use crate::models::Models;
use crate::reasoning::{
    CaptureScope, JobScopes, ReasoningAudit, ReasoningConsumer, SYNC_INTERVAL, SyncConfig,
    SyncWorker, enrich_system_prompt,
};

/// Adapts an [`AuthProvider`] to the client's `BearerProvider` so the sync
/// worker draws a fresh token each attempt without depending on the auth stack.
struct AuthBearer(Arc<dyn AuthProvider>);
impl orkia_reasoning_client::BearerProvider for AuthBearer {
    fn bearer(&self) -> Option<String> {
        self.0.bearer()
    }
}

/// Errors raised while booting intelligence.
#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    #[error("store: {0}")]
    Store(#[from] orkia_reasoning_store::StoreError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sync: {0}")]
    Sync(String),
}

/// Everything needed to bring the reasoning hot path online.
pub struct BootConfig {
    /// Where the local SQLite store lives (created if absent).
    pub store_path: PathBuf,
    /// Workspace/account + session-level project/RFC fallback stamped onto turns.
    pub scope: CaptureScope,
    /// A fresh subscription to the journal broadcast bus.
    pub bus: broadcast::Receiver<JournalEnvelope>,
    /// Shared per-job project/RFC map the REPL writes at spawn (one owner: the
    /// REPL; the consumer only reads).
    pub job_scopes: JobScopes,
    /// Cloud base URL (e.g. from `resolve_backend_url`). Empty ⇒ sync disabled
    /// (the hot path still captures locally; offline-only / tests).
    pub backend_url: String,
    /// SEAL audit sink for consolidated nodes. `None` disables reasoning
    /// audit emission (offline/tests); the shell wires its `EventRouter` impl.
    pub audit: Option<Arc<dyn ReasoningAudit>>,
}

/// Internal bundle for [`Intelligence::boot_sync`], keeping it within the
/// 4-argument limit.
struct SyncBoot {
    backend_url: String,
    store_path: PathBuf,
    workspace_id: Uuid,
    account_id: Uuid,
    audit: Option<Arc<dyn ReasoningAudit>>,
}

/// The Orkia Intelligence handle. One per shell, held by the REPL.
pub struct Intelligence {
    gate: Gate,
    auth: Arc<dyn AuthProvider>,
    classifier: Classifier,
    models: Models,
    prefs: Arc<PreferenceCache>,
    task: Option<JoinHandle<()>>,
    sync_task: Option<JoinHandle<()>>,
    /// Sticky premium-denied flag: set when the server answers `403` to a sync,
    /// surfaced in `$reasoning status`, reset on the next boot (re-login).
    premium_denied: Arc<AtomicBool>,
    /// Manual-sync trigger handed to the sync worker. `$reasoning sync`
    /// notifies it to wake the worker ahead of its interval tick.
    sync_trigger: Arc<Notify>,
    /// Workspace the consumer booted against, remembered so enrich at spawn
    /// can look up the preference cache without the caller re-resolving it.
    active_workspace: Option<Uuid>,
}

impl Intelligence {
    /// Construct an inert handle. Nothing runs until [`Intelligence::boot`].
    pub fn new(auth: Arc<dyn AuthProvider>, rpc: Option<Arc<dyn KernelRpc>>) -> Self {
        Self {
            gate: Gate::new(auth.clone()),
            auth,
            classifier: Classifier::new(rpc.clone()),
            models: Models::new(rpc),
            prefs: Arc::new(PreferenceCache::new()),
            task: None,
            sync_task: None,
            premium_denied: Arc::new(AtomicBool::new(false)),
            sync_trigger: Arc::new(Notify::new()),
            active_workspace: None,
        }
    }

    /// The reasoning identity (workspace + account) for the live session, or
    /// `None` when the gate is closed / no usable identity (fail-closed).
    /// The shell uses this to build the [`BootConfig`] scope.
    pub fn identity(&self) -> Option<Identity> {
        self.gate.identity()
    }

    /// Boot the reasoning subsystem **iff** the gate is open. Returns whether
    /// it actually started (`false` = inert, by design, not an error).
    pub fn boot(&mut self, cfg: BootConfig) -> Result<bool, KernelError> {
        if self.gate.state() != GateState::Enabled {
            tracing::info!(state = ?self.gate.state(), "intelligence: gate closed, staying inert");
            return Ok(false);
        }
        if self.task.is_some() {
            return Ok(true); // already booted; idempotent
        }
        if let Some(parent) = cfg.store_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let store = ReasoningStore::open(&cfg.store_path)?;
        self.warm_cache(&store, cfg.scope.workspace_id)?;
        self.active_workspace = Some(cfg.scope.workspace_id);
        // Fresh boot resets a prior session's sticky premium denial.
        self.premium_denied.store(false, Ordering::Relaxed);
        let boot = SyncBoot {
            backend_url: cfg.backend_url,
            store_path: cfg.store_path.clone(),
            workspace_id: cfg.scope.workspace_id,
            account_id: cfg.scope.account_id,
            audit: cfg.audit,
        };
        let consumer = ReasoningConsumer::with_job_scopes(store, cfg.scope, cfg.job_scopes);
        self.task = Some(tokio::spawn(consumer.run(cfg.bus)));
        self.boot_sync(boot);
        tracing::info!("intelligence: reasoning hot path online");
        Ok(true)
    }

    /// Spawn the cold-path sync worker when a backend URL is configured. Failure
    /// to construct the client/worker is logged, not fatal — the hot path keeps
    /// capturing locally and the queue drains once sync can run.
    fn boot_sync(&mut self, boot: SyncBoot) {
        if boot.backend_url.is_empty() {
            return;
        }
        let bearer = Arc::new(AuthBearer(self.auth.clone()));
        let client = match ReasoningClient::new(boot.backend_url, bearer) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "intelligence: sync client init failed; sync disabled");
                return;
            }
        };
        let worker = SyncWorker::open(SyncConfig {
            store_path: boot.store_path,
            workspace_id: boot.workspace_id,
            account_id: boot.account_id,
            client,
            prefs: Arc::clone(&self.prefs),
            interval: SYNC_INTERVAL,
            premium_denied: Arc::clone(&self.premium_denied),
            trigger: Arc::clone(&self.sync_trigger),
            audit: boot.audit,
        });
        match worker {
            Ok(w) => {
                self.sync_task = Some(tokio::spawn(w.run()));
                tracing::info!("intelligence: reasoning sync worker online");
            }
            Err(e) => tracing::warn!(error = %e, "intelligence: sync worker open failed"),
        }
    }

    /// Tear down the reasoning tasks cleanly (e.g. on `$logout`). Idempotent.
    pub fn shutdown(&mut self) {
        if let Some(task) = self.sync_task.take() {
            task.abort();
        }
        if let Some(task) = self.task.take() {
            task.abort();
            self.active_workspace = None;
            tracing::info!("intelligence: reasoning hot path stopped");
        }
    }

    /// Whether the cloud has answered `403 premium_required` to a sync this
    /// session (sticky until the next boot). Surfaced by `$reasoning status`.
    pub fn premium_denied(&self) -> bool {
        self.premium_denied.load(Ordering::Relaxed)
    }

    /// Ask the sync worker to run a push/pull now instead of waiting for its
    /// next interval tick (backs `$reasoning sync`). A no-op signal when no
    /// worker is running (offline / sync disabled) — the notify is simply not
    /// observed. Returns whether a worker is live to service it.
    pub fn request_sync(&self) -> bool {
        self.sync_trigger.notify_one();
        self.sync_task.is_some()
    }

    /// Whether the background sync worker is running (a backend URL was
    /// configured at boot). Surfaced by `$reasoning status`.
    pub fn sync_active(&self) -> bool {
        self.sync_task.is_some()
    }

    /// Enrich an agent system prompt with preference + reasoning-context
    /// blocks. Synchronous; never touches the network.
    pub fn enrich(
        &self,
        base: &str,
        workspace_id: Uuid,
        context: Option<&ReasoningContext>,
    ) -> String {
        enrich_system_prompt(base, &self.prefs, workspace_id, context)
    }

    /// Enrich against the booted workspace. When the consumer is inert (gate
    /// closed / not booted) this is an exact pass-through — no allocation of a
    /// preference block, never a network call. This is the spawn-path
    /// entry point the REPL calls.
    pub fn enrich_active(&self, base: &str, context: Option<&ReasoningContext>) -> String {
        match self.active_workspace {
            // `active_workspace` is set only when `boot()` saw `GateState::Enabled`
            // (premium), which is exactly when the KG MCP tools are registered.
            // So this branch is the gate-open path: append the L2a Knowledge
            // Protocol block that trains the agent to `recall` before
            // acting. The closed-gate branch omits it (the tool does not exist).
            Some(ws) => {
                let enriched = enrich_system_prompt(base, &self.prefs, ws, context);
                append_knowledge_protocol(&enriched)
            }
            None => base.to_string(),
        }
    }

    pub fn preferences(&self) -> Arc<PreferenceCache> {
        Arc::clone(&self.prefs)
    }

    pub fn classifier(&self) -> &Classifier {
        &self.classifier
    }

    pub fn models(&self) -> &Models {
        &self.models
    }

    pub fn gate_state(&self) -> GateState {
        self.gate.state()
    }

    pub fn is_active(&self) -> bool {
        self.task.is_some()
    }

    fn warm_cache(&self, store: &ReasoningStore, workspace_id: Uuid) -> Result<(), KernelError> {
        let prefs = store.preferences_for_workspace(workspace_id)?;
        if !prefs.is_empty() {
            self.prefs.put(workspace_id, prefs);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use orkia_auth::provider::{AuthError, AuthEventSink, SessionInfo};
    use orkia_rfc_core::id::RfcId;

    struct StubAuth(Option<SessionInfo>);
    impl AuthProvider for StubAuth {
        fn login(&self, _: &mut dyn AuthEventSink) -> Result<SessionInfo, AuthError> {
            Err(AuthError::Cancelled)
        }
        fn logout(&self) -> Result<(), AuthError> {
            Ok(())
        }
        fn current(&self) -> Option<SessionInfo> {
            self.0.clone()
        }
        fn bearer(&self) -> Option<String> {
            None
        }
    }

    fn session(plan: &str) -> SessionInfo {
        SessionInfo {
            display_name: "k".into(),
            email: "k@x.io".into(),
            plan: plan.into(),
            issued_at: Utc::now(),
            expires_at: None,
            account_id: None,
            workspace_id: None,
        }
    }

    fn scope() -> CaptureScope {
        CaptureScope {
            workspace_id: Uuid::from_u128(1),
            account_id: Uuid::from_u128(2),
            project_id: None,
            rfc_ref: Some(orkia_reasoning_core::dto::RfcRef::new(RfcId::new("rfc-1"))),
        }
    }

    type BootParts = (BootConfig, broadcast::Sender<JournalEnvelope>);

    fn boot_cfg(dir: &std::path::Path) -> BootParts {
        let (tx, rx) = broadcast::channel(16);
        let cfg = BootConfig {
            store_path: dir.join("reasoning.db"),
            scope: scope(),
            bus: rx,
            job_scopes: crate::reasoning::new_job_scopes(),
            backend_url: String::new(),
            audit: None,
        };
        (cfg, tx)
    }

    #[tokio::test]
    async fn free_plan_stays_inert() {
        let auth = Arc::new(StubAuth(Some(session("free"))));
        let mut intel = Intelligence::new(auth, None);
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, _tx) = boot_cfg(tmp.path());
        let booted = intel.boot(cfg).unwrap();
        assert!(!booted);
        assert!(!intel.is_active());
        // No store file created on the inert path.
        assert!(!tmp.path().join("reasoning.db").exists());
    }

    #[tokio::test]
    async fn premium_boots_and_shuts_down() {
        let auth = Arc::new(StubAuth(Some(session("starter"))));
        let mut intel = Intelligence::new(auth, None);
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, _tx) = boot_cfg(tmp.path());
        let booted = intel.boot(cfg).unwrap();
        assert!(booted);
        assert!(intel.is_active());
        assert!(tmp.path().join("reasoning.db").exists());
        intel.shutdown();
        assert!(!intel.is_active());
    }

    #[tokio::test]
    async fn anonymous_enrich_is_passthrough() {
        let auth = Arc::new(StubAuth(None));
        let intel = Intelligence::new(auth, None);
        let out = intel.enrich("base prompt", Uuid::from_u128(1), None);
        assert_eq!(out, "base prompt");
    }

    #[tokio::test]
    async fn enrich_active_appends_l2a_only_when_gate_open() {
        // Gate closed (no session): spawn-path enrich is an exact pass-through,
        // and must NOT reference the premium-only `recall` tool.
        let closed = Intelligence::new(Arc::new(StubAuth(None)), None);
        let out = closed.enrich_active("base prompt", None);
        assert_eq!(out, "base prompt");
        assert!(!out.contains("Knowledge Protocol"));

        // Gate open (premium boot): the L2a block is appended.
        let auth = Arc::new(StubAuth(Some(session("starter"))));
        let mut intel = Intelligence::new(auth, None);
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, _tx) = boot_cfg(tmp.path());
        assert!(intel.boot(cfg).unwrap());
        let out = intel.enrich_active("base prompt", None);
        assert!(out.contains("## Knowledge Protocol"));
        assert!(out.contains("`recall` tool"));
        intel.shutdown();
    }
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! SyncWorker against a real axum loopback server: a dirty turn pushes and
//! clears; a 403 sets the sticky premium flag; pulled nodes/preferences land in
//! the store and refresh the cache.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use uuid::Uuid;

use orkia_reasoning_client::ReasoningClient;
use orkia_reasoning_core::PreferenceCache;
use orkia_reasoning_core::dto::{RfcRef, TurnDto};
use orkia_reasoning_core::enums::{TurnKind, TurnRole};
use orkia_reasoning_store::{NewSession, ReasoningStore, TurnInsert};
use orkia_rfc_core::id::RfcId;

use super::*;

const WS: u128 = 1;
const ACC: u128 = 2;
const PROJ: u128 = 3;

struct Tok;
impl orkia_reasoning_client::BearerProvider for Tok {
    fn bearer(&self) -> Option<String> {
        Some("t".into())
    }
}

async fn spawn(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

/// Seed a store at `path` with one open session + one dirty tool-call turn.
fn seed_dirty_turn(path: &std::path::Path) -> Uuid {
    let store = ReasoningStore::open(path).unwrap();
    let sid = store
        .create_session(&NewSession {
            workspace_id: Uuid::from_u128(WS),
            account_id: Uuid::from_u128(ACC),
            agent_name: "faye".into(),
            project_id: Some(Uuid::from_u128(PROJ)),
            rfc_ref: Some(RfcRef::new(RfcId::new("rfc-9"))),
        })
        .unwrap();
    let dto = TurnDto {
        client_event_id: Uuid::from_u128(99),
        session_id: Some(sid),
        workspace_id: Uuid::from_u128(WS),
        project_id: Some(Uuid::from_u128(PROJ)),
        rfc_ref: Some(RfcRef::new(RfcId::new("rfc-9"))),
        agent_name: "faye".into(),
        role: TurnRole::Tool,
        kind: TurnKind::ToolCall("Bash".into()),
        summary: "ran build".into(),
        content_hash: "h".into(),
        token_count: None,
        metadata: serde_json::Value::Null,
        parent_turn_id: None,
        relation: None,
        occurred_at: Utc::now(),
    };
    store
        .insert_turn(&TurnInsert {
            dto: &dto,
            seq: 1,
            thinking_trace: None,
            thinking_tokens: None,
        })
        .unwrap();
    sid
}

fn worker(path: std::path::PathBuf, base: String, denied: Arc<AtomicBool>) -> SyncWorker {
    let client = ReasoningClient::new(base, Arc::new(Tok)).unwrap();
    SyncWorker::open(SyncConfig {
        store_path: path,
        workspace_id: Uuid::from_u128(WS),
        account_id: Uuid::from_u128(ACC),
        client,
        prefs: Arc::new(PreferenceCache::new()),
        interval: Duration::from_secs(30),
        premium_denied: denied,
        trigger: Arc::new(tokio::sync::Notify::new()),
        audit: None,
    })
    .unwrap()
}

#[tokio::test]
async fn push_clears_dirty_turn_on_accept() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("reasoning.db");
    seed_dirty_turn(&path);

    async fn handler() -> impl IntoResponse {
        Json(serde_json::json!({ "accepted": 1, "errors": 0 }))
    }
    let base = spawn(Router::new().route("/v1/reasoning/sync", post(handler))).await;
    let mut w = worker(path.clone(), base, Arc::new(AtomicBool::new(false)));

    assert_eq!(w.store.dirty_turn_dtos(10).unwrap().len(), 1);
    w.push().await;
    assert!(
        w.store.dirty_turn_dtos(10).unwrap().is_empty(),
        "accepted turn must be cleared"
    );
}

#[tokio::test]
async fn push_403_sets_sticky_premium_flag_and_keeps_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("reasoning.db");
    seed_dirty_turn(&path);

    async fn handler() -> impl IntoResponse {
        (StatusCode::FORBIDDEN, "premium_required")
    }
    let base = spawn(Router::new().route("/v1/reasoning/sync", post(handler))).await;
    let denied = Arc::new(AtomicBool::new(false));
    let mut w = worker(path.clone(), base, denied.clone());

    w.push().await;
    assert!(denied.load(Ordering::Relaxed), "403 sets sticky flag");
    // Rows stay dirty — nothing was accepted.
    assert_eq!(w.store.dirty_turn_dtos(10).unwrap().len(), 1);
}

#[tokio::test]
async fn pull_writes_nodes_and_refreshes_pref_cache() {
    use orkia_reasoning_core::enums::{Dimension, KnowledgeNodeKind, NodeOrigin, PreferenceScope};

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("reasoning.db");
    // Touch the store so the file + schema exist before the worker opens it.
    ReasoningStore::open(&path).unwrap();

    let node = orkia_reasoning_core::dto::KnowledgeNode {
        id: Uuid::from_u128(0xBEEF),
        workspace_id: Uuid::from_u128(WS),
        project_id: Some(Uuid::from_u128(PROJ)),
        rfc_ref: None,
        kind: KnowledgeNodeKind::Decision,
        summary: "use sqlite".into(),
        confidence: 0.9,
        origin: NodeOrigin::Cloud,
        created_at: Utc::now(),
    };
    let pref = orkia_reasoning_core::dto::PreferenceDto {
        dimension: Dimension::Verbosity,
        value: "concise".into(),
        confidence: 0.8,
        observation_count: 3,
        scope: PreferenceScope::Workspace,
    };
    let node_h = node.clone();
    let pref_h = pref.clone();
    let app = Router::new()
        .route(
            "/v1/reasoning/nodes/:ws",
            get(move || {
                let node = node_h.clone();
                async move { Json(serde_json::json!({ "nodes": [node] })) }
            }),
        )
        .route(
            "/v1/reasoning/preferences/:ws",
            get(move || {
                let pref = pref_h.clone();
                async move { Json(serde_json::json!({ "preferences": [pref] })) }
            }),
        );
    let base = spawn(app).await;

    let prefs_cache = Arc::new(PreferenceCache::new());
    let client = ReasoningClient::new(base, Arc::new(Tok)).unwrap();
    let audit = Arc::new(CaptureAudit::default());
    let mut w = SyncWorker::open(SyncConfig {
        store_path: path,
        workspace_id: Uuid::from_u128(WS),
        account_id: Uuid::from_u128(ACC),
        client,
        prefs: prefs_cache.clone(),
        interval: Duration::from_secs(30),
        premium_denied: Arc::new(AtomicBool::new(false)),
        trigger: Arc::new(tokio::sync::Notify::new()),
        audit: Some(audit.clone()),
    })
    .unwrap();

    w.pull().await;

    let nodes = w.store.nodes_for_project(Uuid::from_u128(PROJ)).unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].id, Uuid::from_u128(0xBEEF));
    let cached = prefs_cache.get(Uuid::from_u128(WS)).unwrap();
    assert_eq!(cached.len(), 1);
    assert_eq!(cached[0].value, "concise");
    // Pulled prefs also persisted to the store.
    assert_eq!(
        w.store
            .preferences_for_workspace(Uuid::from_u128(WS))
            .unwrap()
            .len(),
        1
    );
    // consolidating a node emits exactly one audit batch naming its id.
    let batches = audit.batches.lock().unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0], vec![Uuid::from_u128(0xBEEF)]);
}

#[derive(Default)]
struct CaptureAudit {
    batches: std::sync::Mutex<Vec<Vec<Uuid>>>,
}
impl ReasoningAudit for CaptureAudit {
    fn nodes_consolidated(&self, node_ids: &[Uuid], _rfc_id: Option<&str>) {
        self.batches.lock().unwrap().push(node_ids.to_vec());
    }
}

#[tokio::test]
async fn manual_trigger_wakes_worker_before_interval() {
    // A long interval ensures the worker would not push on its own within the
    // test window; the `$reasoning sync` trigger must wake it.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("reasoning.db");
    seed_dirty_turn(&path);

    async fn handler() -> impl IntoResponse {
        Json(serde_json::json!({ "accepted": 1, "errors": 0 }))
    }
    let base = spawn(Router::new().route("/v1/reasoning/sync", post(handler))).await;
    let client = ReasoningClient::new(base, Arc::new(Tok)).unwrap();
    let trigger = Arc::new(tokio::sync::Notify::new());
    let w = SyncWorker::open(SyncConfig {
        store_path: path.clone(),
        workspace_id: Uuid::from_u128(WS),
        account_id: Uuid::from_u128(ACC),
        client,
        prefs: Arc::new(PreferenceCache::new()),
        interval: Duration::from_secs(3600),
        premium_denied: Arc::new(AtomicBool::new(false)),
        trigger: trigger.clone(),
        audit: None,
    })
    .unwrap();

    let task = tokio::spawn(w.run());
    // Let the worker reach its select before notifying (notify_one has no
    // permit-stacking guarantee across a not-yet-awaited notified()).
    tokio::time::sleep(Duration::from_millis(50)).await;
    trigger.notify_one();

    // Poll a reader connection until the dirty turn clears (push succeeded).
    let reader = ReasoningStore::open(&path).unwrap();
    let mut cleared = false;
    for _ in 0..50 {
        if reader.dirty_turn_dtos(10).unwrap().is_empty() {
            cleared = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    task.abort();
    assert!(cleared, "manual trigger must push within the window");
}

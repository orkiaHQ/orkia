// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! Loopback tests against a real axum server (the established pattern in
//! `orkia-stream`) — exercises serialization, status classification, the
//! idempotency header, and the scoped GET query string end to end.

use std::sync::Arc;

use axum::extract::Query;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use uuid::Uuid;

use orkia_reasoning_core::dto::{KnowledgeNode, PreferenceDto, RfcRef, TurnDto};
use orkia_reasoning_core::enums::{
    Dimension, KnowledgeNodeKind, NodeOrigin, PreferenceScope, TurnKind, TurnRole,
};
use orkia_rfc_core::id::RfcId;

use super::*;

struct Tok(Option<String>);
impl BearerProvider for Tok {
    fn bearer(&self) -> Option<String> {
        self.0.clone()
    }
}

fn client(base: String, token: Option<&str>) -> ReasoningClient {
    ReasoningClient::new(base, Arc::new(Tok(token.map(str::to_string)))).unwrap()
}

async fn spawn(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

fn sample_turn() -> TurnDto {
    TurnDto {
        client_event_id: Uuid::from_u128(7),
        session_id: Some(Uuid::from_u128(2)),
        workspace_id: Uuid::from_u128(1),
        project_id: Some(Uuid::from_u128(3)),
        rfc_ref: Some(RfcRef::new(RfcId::new("rfc-9"))),
        agent_name: "faye".into(),
        role: TurnRole::Agent,
        kind: TurnKind::ToolCall("Bash".into()),
        summary: "ran build".into(),
        content_hash: "h".into(),
        token_count: Some(5),
        metadata: serde_json::Value::Null,
        parent_turn_id: None,
        relation: None,
        occurred_at: Utc::now(),
    }
}

fn batch() -> SyncBatch {
    SyncBatch {
        turns: vec![sample_turn()],
        signals: vec![],
    }
}

#[test]
fn fetch_scope_apply_to_emits_only_set_fields() {
    let empty = FetchScope::default().apply_to("http://h/x").unwrap();
    assert_eq!(empty.query(), None);

    let scope = FetchScope {
        since: None,
        project_id: Some(Uuid::from_u128(0xC0FFEE)),
        rfc_id: Some("rfc-9".into()),
    };
    let url = scope.apply_to("http://h/x").unwrap();
    let q = url.query().unwrap();
    assert!(q.contains(&format!("project_id={}", Uuid::from_u128(0xC0FFEE))));
    assert!(q.contains("rfc_id=rfc-9"));
    assert!(!q.contains("since="));
}

#[tokio::test]
async fn empty_batch_skips_request() {
    // No server bound — if this hit the network it would error/hang.
    let c = client("http://127.0.0.1:1".into(), Some("t"));
    let out = c.sync_batch(&SyncBatch::default()).await.unwrap();
    assert_eq!(out, SyncOutcome::Accepted { accepted: 0 });
}

#[tokio::test]
async fn missing_bearer_is_auth_expired() {
    let c = client("http://127.0.0.1:1".into(), None);
    let out = c.sync_batch(&batch()).await.unwrap();
    assert_eq!(out, SyncOutcome::AuthExpired);
}

#[tokio::test]
async fn sync_happy_path_requires_idempotency_key() {
    async fn handler(headers: HeaderMap, body: String) -> impl IntoResponse {
        // The body must carry our turn, and the idempotency key must be present.
        assert!(body.contains("\"kind\""), "turn serialized into body");
        if headers.get("x-idempotency-key").is_none() {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({}))).into_response();
        }
        Json(serde_json::json!({ "accepted": 1, "errors": 0 })).into_response()
    }
    let base = spawn(Router::new().route("/v1/reasoning/sync", post(handler))).await;
    let out = client(base, Some("t")).sync_batch(&batch()).await.unwrap();
    assert_eq!(out, SyncOutcome::Accepted { accepted: 1 });
}

#[tokio::test]
async fn sync_403_is_premium_required() {
    async fn handler() -> impl IntoResponse {
        (StatusCode::FORBIDDEN, "premium_required")
    }
    let base = spawn(Router::new().route("/v1/reasoning/sync", post(handler))).await;
    let out = client(base, Some("t")).sync_batch(&batch()).await.unwrap();
    assert_eq!(out, SyncOutcome::PremiumRequired);
}

#[tokio::test]
async fn sync_401_is_auth_expired() {
    async fn handler() -> impl IntoResponse {
        (StatusCode::UNAUTHORIZED, "expired")
    }
    let base = spawn(Router::new().route("/v1/reasoning/sync", post(handler))).await;
    let out = client(base, Some("t")).sync_batch(&batch()).await.unwrap();
    assert_eq!(out, SyncOutcome::AuthExpired);
}

#[tokio::test]
async fn sync_400_is_dropped() {
    async fn handler() -> impl IntoResponse {
        (StatusCode::BAD_REQUEST, "bad")
    }
    let base = spawn(Router::new().route("/v1/reasoning/sync", post(handler))).await;
    let out = client(base, Some("t")).sync_batch(&batch()).await.unwrap();
    assert_eq!(out, SyncOutcome::Dropped);
}

#[tokio::test]
async fn fetch_nodes_decodes_and_forwards_scope() {
    let node = KnowledgeNode {
        id: Uuid::from_u128(11),
        workspace_id: Uuid::from_u128(1),
        project_id: Some(Uuid::from_u128(3)),
        rfc_ref: None,
        kind: KnowledgeNodeKind::Decision,
        summary: "use sqlite".into(),
        confidence: 0.9,
        origin: NodeOrigin::Cloud,
        created_at: Utc::now(),
    };
    let node_for_handler = node.clone();
    let handler = move |Query(q): Query<std::collections::HashMap<String, String>>| {
        let node = node_for_handler.clone();
        async move {
            // The scope must reach the server as query params.
            assert_eq!(q.get("project_id"), Some(&Uuid::from_u128(3).to_string()));
            assert_eq!(q.get("rfc_id"), Some(&"rfc-9".to_string()));
            Json(serde_json::json!({ "nodes": [node] }))
        }
    };
    let base = spawn(Router::new().route("/v1/reasoning/nodes/:ws", get(handler))).await;
    let scope = FetchScope {
        since: None,
        project_id: Some(Uuid::from_u128(3)),
        rfc_id: Some("rfc-9".into()),
    };
    let nodes = client(base, Some("t"))
        .fetch_nodes(Uuid::from_u128(1), &scope)
        .await
        .unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].id, node.id);
    assert_eq!(nodes[0].kind, KnowledgeNodeKind::Decision);
}

#[tokio::test]
async fn fetch_preferences_decodes_body() {
    let pref = PreferenceDto {
        dimension: Dimension::Verbosity,
        value: "concise".into(),
        confidence: 0.8,
        observation_count: 3,
        scope: PreferenceScope::Workspace,
    };
    let pref_for_handler = pref.clone();
    let handler = move || {
        let pref = pref_for_handler.clone();
        async move { Json(serde_json::json!({ "preferences": [pref] })) }
    };
    let base = spawn(Router::new().route("/v1/reasoning/preferences/:ws", get(handler))).await;
    let prefs = client(base, Some("t"))
        .fetch_preferences(Uuid::from_u128(1), &FetchScope::default())
        .await
        .unwrap();
    assert_eq!(prefs, vec![pref]);
}

#[tokio::test]
async fn fetch_non_success_is_empty_not_error() {
    async fn handler() -> impl IntoResponse {
        (StatusCode::NOT_FOUND, "nope")
    }
    let base = spawn(Router::new().route("/v1/reasoning/nodes/:ws", get(handler))).await;
    let nodes = client(base, Some("t"))
        .fetch_nodes(Uuid::from_u128(1), &FetchScope::default())
        .await
        .unwrap();
    assert!(nodes.is_empty());
}

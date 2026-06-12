// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//!
//! Every byte of `raw` is untrusted (it comes from an agent process): the frame
//! is length-capped, parsed defensively, `limit` is bounded, and a malformed
//! request yields an error response — never a panic (CLAUDE.md #7).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use orkia_reasoning_core::{KnowledgeNode, compile_context_block};
use orkia_reasoning_store::ReasoningStore;

use crate::rpc::{
    INVALID_PARAMS, JsonRpcRequest, JsonRpcResponse, KG_ERROR, METHOD_NOT_FOUND, PARSE_ERROR,
};

/// Default and hard-cap result sizes. The cap bounds both work and the bytes
/// shipped back into the agent's context.
const DEFAULT_LIMIT: usize = 8;
const MAX_LIMIT: usize = 50;
/// How many candidate nodes to scan before query-filtering. Bounds the read.
const CANDIDATE_SCAN: usize = 200;

/// A dispatched call: the JSON-RPC response to ship back to the agent, plus the
/// ids of nodes that were *served* on the read path. The store handle is
/// **read-only** — the REPL-owned consumer is the single logical writer and owns
/// transport journals a `KnowledgeAccess` event for [`Dispatched::accessed`] and
/// the consumer applies the bump; nothing here writes.
#[derive(Debug)]
pub struct Dispatched {
    pub response: JsonRpcResponse,
    pub accessed: Vec<Uuid>,
}

impl Dispatched {
    /// A response that served no node (errors, empty results, `knowledge_search`).
    fn pure(response: JsonRpcResponse) -> Self {
        Self {
            response,
            accessed: Vec::new(),
        }
    }
}

/// Top-level dispatch. Read-only over the local cache; never writes, never calls
/// the network, never panics on a malformed frame (CLAUDE.md #7). Served node
/// ids ride out on [`Dispatched::accessed`] for the transport to journal.
pub fn dispatch_request(store: &ReasoningStore, raw: &str) -> Dispatched {
    if let Err(e) = orkia_shell_types::input_limits::check_len(
        raw.as_bytes(),
        orkia_shell_types::input_limits::MCP_FRAME_MAX_BYTES,
        "orkia-knowledge-mcp",
    ) {
        return Dispatched::pure(JsonRpcResponse::err(
            None,
            PARSE_ERROR,
            format!("input rejected: {e}"),
            None,
        ));
    }
    let req: JsonRpcRequest = match serde_json::from_str(raw) {
        Ok(r) => r,
        Err(e) => {
            return Dispatched::pure(JsonRpcResponse::err(
                None,
                PARSE_ERROR,
                format!("parse error: {e}"),
                None,
            ));
        }
    };
    let id = req.id.clone();
    match req.method.as_str() {
        "recall" => recall(store, id, req.params),
        "knowledge_search" => Dispatched::pure(search(store, id, req.params)),
        "knowledge_node" => node(store, id, req.params),
        other => Dispatched::pure(JsonRpcResponse::err(
            id,
            METHOD_NOT_FOUND,
            format!("unknown method: {other}"),
            None,
        )),
    }
}

// ── Params ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RecallParams {
    #[serde(default)]
    query: String,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct SearchParams {
    #[serde(default)]
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct NodeParams {
    id: String,
}

// ── Views (wire output) ────────────────────────────────────────────────

/// Full recall result: the deterministic injection block plus its fields.
#[derive(Debug, Serialize)]
struct NodeView {
    id: String,
    kind: String,
    confidence: f32,
    summary: String,
    context_block: String,
}

/// Lighter search hit — no rendered block (answers "is there anything about X?").
#[derive(Debug, Serialize)]
struct NodeHit {
    id: String,
    kind: String,
    confidence: f32,
    summary: String,
}

impl NodeView {
    fn of(n: &KnowledgeNode) -> Self {
        Self {
            id: n.id.to_string(),
            kind: n.kind.as_str().to_string(),
            confidence: n.confidence,
            summary: n.summary.clone(),
            context_block: compile_context_block(n),
        }
    }
}

// ── Handlers ───────────────────────────────────────────────────────────

fn recall(
    store: &ReasoningStore,
    id: Option<serde_json::Value>,
    params: serde_json::Value,
) -> Dispatched {
    let p: RecallParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            return Dispatched::pure(JsonRpcResponse::err(
                id,
                INVALID_PARAMS,
                format!("invalid params: {e}"),
                None,
            ));
        }
    };
    let project = match p.project.as_deref().map(Uuid::parse_str).transpose() {
        Ok(u) => u,
        Err(e) => {
            return Dispatched::pure(JsonRpcResponse::err(
                id,
                INVALID_PARAMS,
                format!("bad project uuid: {e}"),
                None,
            ));
        }
    };
    let candidates = match candidates(store, p.domain.as_deref(), project) {
        Ok(c) => c,
        Err(e) => return Dispatched::pure(JsonRpcResponse::err(id, KG_ERROR, e, None)),
    };
    let limit = p.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let hits = filter_by_query(candidates, &p.query, limit);
    let accessed: Vec<Uuid> = hits.iter().map(|n| n.id).collect();
    let views: Vec<NodeView> = hits.iter().map(NodeView::of).collect();
    Dispatched {
        response: respond(id, serde_json::json!({ "results": views })),
        accessed,
    }
}

fn search(
    store: &ReasoningStore,
    id: Option<serde_json::Value>,
    params: serde_json::Value,
) -> JsonRpcResponse {
    let p: SearchParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            return JsonRpcResponse::err(id, INVALID_PARAMS, format!("invalid params: {e}"), None);
        }
    };
    let candidates = match store.recent_nodes(CANDIDATE_SCAN) {
        Ok(c) => c,
        Err(e) => return JsonRpcResponse::err(id, KG_ERROR, e.to_string(), None),
    };
    let limit = p.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let hits = filter_by_query(candidates, &p.query, limit);
    let out: Vec<NodeHit> = hits
        .iter()
        .map(|n| NodeHit {
            id: n.id.to_string(),
            kind: n.kind.as_str().to_string(),
            confidence: n.confidence,
            summary: n.summary.clone(),
        })
        .collect();
    respond(id, serde_json::json!({ "results": out }))
}

fn node(
    store: &ReasoningStore,
    id: Option<serde_json::Value>,
    params: serde_json::Value,
) -> Dispatched {
    let p: NodeParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            return Dispatched::pure(JsonRpcResponse::err(
                id,
                INVALID_PARAMS,
                format!("invalid params: {e}"),
                None,
            ));
        }
    };
    let uuid = match Uuid::parse_str(&p.id) {
        Ok(u) => u,
        Err(e) => {
            return Dispatched::pure(JsonRpcResponse::err(
                id,
                INVALID_PARAMS,
                format!("bad id: {e}"),
                None,
            ));
        }
    };
    match store.node_by_id(uuid) {
        Ok(Some(n)) => Dispatched {
            response: respond(id, serde_json::json!({ "node": NodeView::of(&n) })),
            accessed: vec![n.id],
        },
        Ok(None) => Dispatched::pure(respond(
            id,
            serde_json::json!({ "node": serde_json::Value::Null }),
        )),
        Err(e) => Dispatched::pure(JsonRpcResponse::err(id, KG_ERROR, e.to_string(), None)),
    }
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Candidate set before query-filtering: domain-scoped > project-scoped >
/// recent. Cache-only — no network, so a cold cache simply yields fewer rows.
fn candidates(
    store: &ReasoningStore,
    domain: Option<&str>,
    project: Option<Uuid>,
) -> Result<Vec<KnowledgeNode>, String> {
    let r = match (domain, project) {
        (Some(d), _) => store.nodes_for_domain(d, CANDIDATE_SCAN),
        (None, Some(p)) => store.nodes_for_project(p),
        (None, None) => store.recent_nodes(CANDIDATE_SCAN),
    };
    r.map_err(|e| e.to_string())
}

/// Case-insensitive substring match on `summary`, capped at `limit`. An empty
/// query keeps candidate order (confidence/recency) and just truncates.
fn filter_by_query(nodes: Vec<KnowledgeNode>, query: &str, limit: usize) -> Vec<KnowledgeNode> {
    let q = query.trim().to_lowercase();
    nodes
        .into_iter()
        .filter(|n| q.is_empty() || n.summary.to_lowercase().contains(&q))
        .take(limit)
        .collect()
}

fn respond(id: Option<serde_json::Value>, value: serde_json::Value) -> JsonRpcResponse {
    JsonRpcResponse::ok(id, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use orkia_reasoning_core::dto::RfcRef;
    use orkia_reasoning_core::enums::{KnowledgeNodeKind, NodeOrigin};
    use orkia_reasoning_store::NodeInsert;
    use orkia_rfc_core::id::RfcId;

    fn seed(store: &ReasoningStore, summary: &str, conf: f32) -> Uuid {
        let n = KnowledgeNode {
            id: Uuid::new_v4(),
            workspace_id: Uuid::from_u128(1),
            project_id: Some(Uuid::from_u128(3)),
            rfc_ref: Some(RfcRef::new(RfcId::new("rfc-1"))),
            kind: KnowledgeNodeKind::Decision,
            summary: summary.into(),
            confidence: conf,
            origin: NodeOrigin::Cloud,
            created_at: Utc::now(),
        };
        let id = n.id;
        store
            .upsert_node(&NodeInsert {
                node: &n,
                details: None,
                domain: None,
                context_block: None,
                source_turn_id: None,
                source_session_id: None,
                seal_id: None,
            })
            .unwrap();
        id
    }

    fn dispatch(store: &ReasoningStore, method: &str, params: serde_json::Value) -> Dispatched {
        let raw = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": method, "params": params
        })
        .to_string();
        dispatch_request(store, &raw)
    }

    fn call(store: &ReasoningStore, method: &str, params: serde_json::Value) -> serde_json::Value {
        let d = dispatch(store, method, params);
        assert!(
            d.response.error.is_none(),
            "unexpected error: {:?}",
            d.response.error
        );
        d.response.result.unwrap()
    }

    #[test]
    fn recall_filters_by_query_and_renders_block() {
        let store = ReasoningStore::in_memory().unwrap();
        seed(&store, "Auth uses PKCE for the device flow", 0.9);
        seed(&store, "Sync uses a WAL cursor", 0.8);

        let d = dispatch(&store, "recall", serde_json::json!({ "query": "pkce" }));
        let out = d.response.result.as_ref().unwrap();
        let results = out["results"].as_array().unwrap();
        // Only the matching node comes back, with its deterministic block.
        assert_eq!(results.len(), 1);
        assert!(
            results[0]["context_block"]
                .as_str()
                .unwrap()
                .contains("PKCE")
        );
        // The served node id rides out for the transport to journal as a
        // KnowledgeAccess event (the consumer applies the bump).
        assert_eq!(d.accessed.len(), 1);
        assert_eq!(
            d.accessed[0].to_string(),
            results[0]["id"].as_str().unwrap()
        );
    }

    #[test]
    fn knowledge_node_returns_one_or_null() {
        let store = ReasoningStore::in_memory().unwrap();
        let id = seed(&store, "A decision", 0.7);
        let out = call(
            &store,
            "knowledge_node",
            serde_json::json!({ "id": id.to_string() }),
        );
        assert_eq!(out["node"]["id"].as_str().unwrap(), id.to_string());

        let missing = Uuid::new_v4();
        let out = call(
            &store,
            "knowledge_node",
            serde_json::json!({ "id": missing.to_string() }),
        );
        assert!(out["node"].is_null());
    }

    #[test]
    fn unknown_method_and_malformed_frame_fail_closed() {
        let store = ReasoningStore::in_memory().unwrap();
        let d = dispatch_request(&store, r#"{"jsonrpc":"2.0","id":1,"method":"nope"}"#);
        assert_eq!(d.response.error.unwrap().code, METHOD_NOT_FOUND);
        assert!(d.accessed.is_empty());

        // Not JSON at all — a parse error, never a panic.
        let d = dispatch_request(&store, "}{ not json");
        assert_eq!(d.response.error.unwrap().code, PARSE_ERROR);

        // Bad uuid in params → invalid params.
        let d = dispatch_request(
            &store,
            r#"{"jsonrpc":"2.0","id":1,"method":"knowledge_node","params":{"id":"not-a-uuid"}}"#,
        );
        assert_eq!(d.response.error.unwrap().code, INVALID_PARAMS);
    }

    #[test]
    fn search_is_lighter_and_omits_context_block() {
        let store = ReasoningStore::in_memory().unwrap();
        seed(&store, "Cage uses Seatbelt on macOS", 0.9);
        let d = dispatch(
            &store,
            "knowledge_search",
            serde_json::json!({ "query": "seatbelt" }),
        );
        // search is an existence probe, not an access — it never bumps decay.
        assert!(d.accessed.is_empty());
        let out = d.response.result.as_ref().unwrap();
        let results = out["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].get("context_block").is_none());
        assert_eq!(
            results[0]["summary"].as_str().unwrap(),
            "Cage uses Seatbelt on macOS"
        );
    }
}

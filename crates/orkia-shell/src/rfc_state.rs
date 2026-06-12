// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Bridge between the REPL and the filesystem-backed `RfcStateService`.
//!
//! Construction is per-call (cheap: no I/O until a method is invoked).
//! Events emitted by the service are forwarded to the unified
//! `EventRouter::on_custom` channel so existing consumers
//! (`SealManager`, journal store, surface listeners) pick them up
//! through the same path as `rfc.create` and friends.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use orkia_reasoning_core::compile_context_block;
use orkia_rfc_core::{DecisionId, RfcStore};
use orkia_rfc_state::{EventSink, RfcEvent, RfcStateService};
use uuid::Uuid;

use crate::JobId;
use crate::knowledge_activity::{ActiveDomain, KnowledgeActivityHandle};
use crate::protocol::EventRouter;

/// Sink that buffers `RfcEvent`s in memory; the REPL drains them and forwards
/// each through `EventRouter::on_custom` after the service call returns.
///
/// We don't hold a reference to `EventRouter` directly because `EventSink`
/// requires `Send + Sync + 'static`-style ownership for `Box<dyn …>` and the
/// router is borrowed by the REPL. Buffering + drain is simpler and keeps the
/// event ordering deterministic.
#[derive(Debug, Default)]
pub struct BufferingSink {
    events: Mutex<Vec<RfcEvent>>,
}

impl BufferingSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn drain(&self) -> Vec<RfcEvent> {
        match self.events.lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            Err(_) => Vec::new(),
        }
    }
}

impl EventSink for BufferingSink {
    fn emit(&self, event: RfcEvent) {
        if let Ok(mut g) = self.events.lock() {
            g.push(event);
        }
    }
}

/// Forward every event in `events` to the unified event channel as a
/// `rfc.<kind>` custom payload. `project` is included for the SEAL consumer
/// which routes by project chain.
pub fn forward_events(events: Vec<RfcEvent>, router: &EventRouter, project: &str) {
    for ev in events {
        let name = ev.name();
        let rfc_id = ev.rfc_id().clone();
        let mut data = match serde_json::to_value(&ev) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(obj) = data.as_object_mut() {
            obj.insert(
                "project".into(),
                serde_json::Value::String(project.to_string()),
            );
        }
        // Tag the record with this event's RFC id so the SEAL v1
        // assembler can collect it under the right RFC document at
        router.on_custom_with_rfc(JobId(0), "", name, data, Some(rfc_id));
    }
}

/// Build a fresh service rooted at `project_dir`. Used by the cache below
/// when no entry exists yet, and by tests that need an isolated instance.
pub fn make_service(project_dir: &Path) -> (RfcStateService, std::sync::Arc<BufferingSink>) {
    let sink = std::sync::Arc::new(BufferingSink::new());
    let sink_box: Box<dyn EventSink> = Box::new(SharedSink(sink.clone()));
    let store = RfcStore::new(project_dir.to_path_buf());
    (RfcStateService::new(store, sink_box), sink)
}

/// Per-project cache of `RfcStateService` instances. Shared by both the
/// REPL (typed commands) and the MCP dispatcher (agent calls) so they
/// agree on lock state — without this, an agent's `propose_edit` and a
/// human-typed `rfc state` would see independent in-memory lock stores.
/// Holds an `Arc<RfcStateService>` (services are `Send + Sync`) keyed by
/// project name. The background reaper task iterates these to expire
/// idle locks.
///
/// # Arc<Mutex> exception (CLAUDE.md non-negotiable #2)
///
/// `inner` uses `Mutex<HashMap<…>>` shared across the REPL and MCP
/// dispatcher threads. This is the deliberate exception to "one owner per
/// resource": the RFC service cache *must* be coherent between the two
/// call sites — a channel-based design would require round-trip latency
/// on every `propose_edit` or `rfc state` call. The critical section is
/// tiny (a single `HashMap` lookup or insert), so the contention risk is
/// negligible.
#[derive(Default)]
pub struct RfcServiceCache {
    inner: std::sync::Mutex<std::collections::HashMap<String, RfcServiceEntry>>,
}

#[derive(Clone)]
pub struct RfcServiceEntry {
    pub service: std::sync::Arc<RfcStateService>,
    pub sink: std::sync::Arc<BufferingSink>,
    pub project_dir: std::path::PathBuf,
}

impl RfcServiceCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lazy-init: returns the cached entry for `project_name`, building a
    /// fresh service rooted at `project_dir` if this is the first call.
    pub fn get_or_create(&self, project_name: &str, project_dir: &Path) -> RfcServiceEntry {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(existing) = g.get(project_name) {
            return existing.clone();
        }
        let (svc, sink) = make_service(project_dir);
        let entry = RfcServiceEntry {
            service: std::sync::Arc::new(svc),
            sink,
            project_dir: project_dir.to_path_buf(),
        };
        g.insert(project_name.to_string(), entry.clone());
        entry
    }

    /// Snapshot of all currently-cached `(project_name, entry)` pairs.
    /// The reaper iterates this to call `reap_expired_locks` on each.
    pub fn snapshot(&self) -> Vec<(String, RfcServiceEntry)> {
        match self.inner.lock() {
            Ok(g) => g.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// Background reaper. Spawned by the REPL on boot; iterates the cache every
/// `interval` and calls `reap_expired_locks` on each service. Released
/// locks emit `rfc.unlocked` events through the existing per-service sink;
/// `forward_events` routes them through the `EventRouter` so SEAL consumers
/// pick them up alongside other RFC events.
pub fn spawn_lock_reaper(
    cache: std::sync::Arc<RfcServiceCache>,
    router: EventRouter,
    interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        // Skip the first immediate fire so we don't reap before anything
        // has been created.
        tick.tick().await;
        loop {
            tick.tick().await;
            let now = std::time::SystemTime::now();
            for (project, entry) in cache.snapshot() {
                let released = entry.service.reap_expired_locks(now);
                if released > 0 {
                    forward_events(entry.sink.drain(), &router, &project);
                }
            }
        }
    })
}

struct SharedSink(std::sync::Arc<BufferingSink>);
impl EventSink for SharedSink {
    fn emit(&self, e: RfcEvent) {
        self.0.emit(e);
    }
}

/// Maps decision ids to the PTY job that asked them, so resolution can be
///
/// The mapping is in-process and per-session — agent restarts invalidate
/// entries, which is the correct behavior (a dead agent has nothing to
/// inject into). The REPL is the sole owner; concurrent access uses a
/// regular `Mutex` because the critical section is tiny.
///
/// # Arc<Mutex> exception (CLAUDE.md non-negotiable #2)
///
/// `pending` is a `Mutex<HashMap<…>>` because `ClarificationPtyBridge` is
/// constructed once and shared by reference between the REPL thread (which
/// records decision ids) and the MCP transport layer (which resolves them).
/// A channel would require a response path that complicates the MCP handler
/// needlessly. The critical section is a single `HashMap` insert or remove —
/// essentially zero contention in practice.
#[derive(Debug, Default)]
pub struct ClarificationPtyBridge {
    pending: Mutex<HashMap<DecisionId, JobId>>,
}

impl ClarificationPtyBridge {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the asking agent's job id when an `orkia_rfc_ask` call comes
    /// in. The transport layer (MCP socket multiplex) provides the job id;
    /// for REPL-driven asks the job id is `None` and no entry is recorded.
    pub fn record(&self, decision_id: DecisionId, job_id: JobId) {
        if let Ok(mut g) = self.pending.lock() {
            g.insert(decision_id, job_id);
        }
    }

    /// Take the recorded job id (one-shot). Returns `None` if the asking
    /// agent has already received its answer or the ask was REPL-local.
    pub fn take(&self, decision_id: &DecisionId) -> Option<JobId> {
        self.pending
            .lock()
            .ok()
            .and_then(|mut g| g.remove(decision_id))
    }

    /// Render the payload the REPL writes into the agent's PTY when a
    /// clarification is resolved. Newline-terminated so the agent's stdin
    /// reader treats it as a complete line.
    pub fn format_resolution(decision_id: &DecisionId, answer: &str) -> Vec<u8> {
        format!("Decision {decision_id} resolved: {answer}\n").into_bytes()
    }
}

/// Implements [`crate::journal::McpDispatcher`] by resolving the project for
/// each JSON-RPC request from the workspace on disk, building a one-shot
/// `RfcStateService`, dispatching the call, and forwarding any emitted
/// `RfcEvent`s through the shared `EventRouter`. Recording of asking-PTY
/// job ids into the `ClarificationPtyBridge` happens here as well, so the
/// REPL's `rfc resolve` can later route answers back to the right agent.
pub struct McpShellDispatcher {
    data_dir: std::path::PathBuf,
    router: EventRouter,
    pty_bridge: std::sync::Arc<ClarificationPtyBridge>,
    cache: std::sync::Arc<RfcServiceCache>,
    activity: Option<KnowledgeActivityHandle>,
}

impl McpShellDispatcher {
    pub fn new(
        data_dir: std::path::PathBuf,
        router: EventRouter,
        pty_bridge: std::sync::Arc<ClarificationPtyBridge>,
        cache: std::sync::Arc<RfcServiceCache>,
        activity: Option<KnowledgeActivityHandle>,
    ) -> Self {
        Self {
            data_dir,
            router,
            pty_bridge,
            cache,
            activity,
        }
    }

    /// Persist a V3 approval request file so the human can authorize an
    /// agent-proposed promote via `approve <job_id>`. Returns Ok on
    /// successful write; failures are best-effort logged but don't
    /// surface back to the agent (the agent's `propose_promote` call has
    /// already succeeded as far as the protocol is concerned).
    fn queue_promote_approval(
        &self,
        job_id: JobId,
        rfc_id: &str,
        project: &str,
        request: &serde_json::Value,
    ) -> std::io::Result<()> {
        let dir = self.data_dir.join("run").join(job_id.0.to_string());
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("approval.request.json");
        let rationale = request
            .get("params")
            .and_then(|p| p.get("rationale"))
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        let agent = request
            .get("params")
            .and_then(|p| p.get("agent"))
            .and_then(|a| a.as_str())
            .unwrap_or("agent")
            .to_string();
        // Metadata carries the context the REPL's approve handler reads
        // to dispatch the actual promote on accept.
        let payload = serde_json::json!({
            "action": "rfc.promote",
            "description": format!(
                "agent {agent} proposes promoting rfc {rfc_id}: {rationale}"
            ),
            "risk": "medium",
            "metadata": {
                "rfc": {
                    "slug": rfc_id,
                    "project": project,
                    "agent": agent,
                }
            }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&payload)?)?;
        Ok(())
    }

    /// Probe the request params for a `rfc_id` field and search the
    /// workspace for the project that owns it. Returns the project name
    /// and on-disk path, or `None` if the rfc isn't found in any project.
    fn resolve_project(&self, rfc_id: &str) -> Option<(String, std::path::PathBuf)> {
        let ws = orkia_shell_types::Workspace::load(&self.data_dir);
        for p in &ws.projects {
            let store = RfcStore::new(p.path.clone());
            if store.load(&orkia_rfc_core::RfcId::new(rfc_id)).is_ok() {
                return Some((p.name.clone(), p.path.clone()));
            }
        }
        None
    }
}

impl crate::journal::McpDispatcher for McpShellDispatcher {
    fn dispatch(&self, line: &str, peer_job_id: Option<JobId>) -> crate::journal::McpReply {
        // Parse just enough to extract `params.rfc_id` for project routing.
        // Full param validation happens inside `orkia_rfc_mcp::dispatch_request`.
        let parsed: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return crate::journal::McpReply::plain(error_response(
                    None,
                    -32700,
                    &format!("parse error: {e}"),
                ));
            }
        };
        let req_id = parsed.get("id").cloned();
        // Knowledge-graph reads ride the same socket but route to the local
        // carry no `rfc_id`, so branch before RFC routing.
        let method = parsed.get("method").and_then(|m| m.as_str());
        if matches!(
            method,
            Some("recall") | Some("knowledge_search") | Some("knowledge_node")
        ) {
            return self.dispatch_knowledge(line, req_id, peer_job_id);
        }
        let rfc_id = parsed
            .get("params")
            .and_then(|p| p.get("rfc_id"))
            .and_then(|v| v.as_str());
        let Some(rfc_id) = rfc_id else {
            return crate::journal::McpReply::plain(error_response(
                req_id,
                -32602,
                "missing params.rfc_id",
            ));
        };
        let Some((project, project_dir)) = self.resolve_project(rfc_id) else {
            return crate::journal::McpReply::plain(error_response(
                req_id,
                -32602,
                &format!("rfc_id '{rfc_id}' not found in any project"),
            ));
        };
        let entry = self.cache.get_or_create(&project, &project_dir);
        let resp = orkia_rfc_mcp::dispatch_request(&entry.service, line);

        // If this was an `orkia_rfc_ask` and we know the asking peer, record
        // the (decision_id → job_id) so resolution can be injected back into
        if method == Some("orkia_rfc_ask")
            && let Some(job_id) = peer_job_id
            && let Some(result) = &resp.result
            && let Some(did) = result.as_str()
        {
            self.pty_bridge
                .record(orkia_rfc_core::DecisionId::new(did), job_id);
        }

        // If this was a successful `orkia_rfc_propose_promote`, queue a
        // human approval via the existing V3 file-based ApprovalWatcher
        // The approval carries enough metadata that the REPL's `approve`
        // handler can later dispatch the actual promote on accept.
        if method == Some("orkia_rfc_propose_promote")
            && resp.error.is_none()
            && let Some(job_id) = peer_job_id
            && let Err(e) = self.queue_promote_approval(job_id, rfc_id, &project, &parsed)
        {
            tracing::warn!(
                job = job_id.0,
                rfc = %rfc_id,
                error = %e,
                "rfc_state: failed to queue promote approval; human approval will not be surfaced",
            );
        }

        forward_events(entry.sink.drain(), &self.router, &project);
        let response = match serde_json::to_value(&resp) {
            Ok(mut value) => {
                let mut accessed = Vec::new();
                self.enrich_response(&mut value, peer_job_id, &[], &mut accessed);
                match serde_json::to_string(&value) {
                    Ok(s) => {
                        return crate::journal::McpReply {
                            response: s,
                            accessed_node_ids: accessed,
                        };
                    }
                    Err(e) => error_response(parsed.get("id").cloned(), -32603, &e.to_string()),
                }
            }
            Err(e) => error_response(parsed.get("id").cloned(), -32603, &e.to_string()),
        };
        crate::journal::McpReply::plain(response)
    }
}

impl McpShellDispatcher {
    /// Serve a knowledge-graph read (`recall` / `knowledge_search` /
    /// `knowledge_node`) from the local reasoning store. Strictly read-only
    /// the access bump is applied by the store-owning consumer, never here.
    /// Fail-closed — a store that won't open returns a JSON-RPC error, no panic.
    fn dispatch_knowledge(
        &self,
        line: &str,
        req_id: Option<serde_json::Value>,
        peer_job_id: Option<JobId>,
    ) -> crate::journal::McpReply {
        let path = crate::reasoning_builtins::store_path(&self.data_dir);
        let store = match orkia_reasoning_store::ReasoningStore::open(&path) {
            Ok(s) => s,
            Err(e) => {
                return crate::journal::McpReply::plain(error_response(
                    req_id,
                    -32603,
                    &format!("knowledge store unavailable: {e}"),
                ));
            }
        };
        let dispatched = orkia_knowledge_mcp::dispatch_request(&store, line);
        let mut accessed_node_ids: Vec<String> = dispatched
            .accessed
            .iter()
            .map(|id| id.to_string())
            .collect();
        let response = match serde_json::to_value(&dispatched.response) {
            Ok(mut value) => {
                self.enrich_response(
                    &mut value,
                    peer_job_id,
                    &dispatched.accessed,
                    &mut accessed_node_ids,
                );
                match serde_json::to_string(&value) {
                    Ok(s) => s,
                    Err(e) => error_response(req_id, -32603, &e.to_string()),
                }
            }
            Err(e) => error_response(req_id, -32603, &e.to_string()),
        };
        crate::journal::McpReply {
            response,
            accessed_node_ids,
        }
    }

    fn enrich_response(
        &self,
        response: &mut serde_json::Value,
        peer_job_id: Option<JobId>,
        existing: &[Uuid],
        accessed: &mut Vec<String>,
    ) {
        if response.get("error").is_some() {
            return;
        }
        let Some(job_id) = peer_job_id else { return };
        let Some(result) = response.get_mut("result") else {
            return;
        };
        let Some(result_obj) = result.as_object_mut() else {
            return;
        };
        let Some(activity) = self.activity.as_ref() else {
            return;
        };
        let domains = activity.active_domains(job_id);
        if domains.is_empty() {
            return;
        }
        let path = crate::reasoning_builtins::store_path(&self.data_dir);
        let Ok(store) = orkia_reasoning_store::ReasoningStore::open(&path) else {
            return;
        };
        let mut excluded: std::collections::HashSet<Uuid> = existing.iter().copied().collect();
        let mut contexts = Vec::new();
        for domain in domains {
            if contexts.len() >= 3 {
                break;
            }
            self.push_domain_context(&store, domain, &mut excluded, &mut contexts, accessed);
        }
        if !contexts.is_empty() {
            result_obj.insert(
                "relevant_context".into(),
                serde_json::Value::Array(contexts),
            );
        }
    }

    fn push_domain_context(
        &self,
        store: &orkia_reasoning_store::ReasoningStore,
        domain: ActiveDomain,
        excluded: &mut std::collections::HashSet<Uuid>,
        contexts: &mut Vec<serde_json::Value>,
        accessed: &mut Vec<String>,
    ) {
        let Ok(nodes) = store.nodes_for_domain(&domain.domain, 3) else {
            return;
        };
        for node in nodes {
            if contexts.len() >= 3 {
                break;
            }
            if !excluded.insert(node.id) {
                continue;
            }
            accessed.push(node.id.to_string());
            contexts.push(serde_json::json!({
                "id": node.id.to_string(),
                "domain": domain.domain.clone(),
                "reason": domain.reason.clone(),
                "context_block": compile_context_block(&node),
            }));
        }
    }
}

fn error_response(id: Option<serde_json::Value>, code: i32, message: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_records_and_takes_once() {
        let b = ClarificationPtyBridge::new();
        let did = DecisionId::new("d-001");
        b.record(did.clone(), JobId(42));
        assert_eq!(b.take(&did), Some(JobId(42)));
        // One-shot: second take returns None.
        assert_eq!(b.take(&did), None);
    }

    /// End-to-end Gap #1 closure: an `orkia_rfc_ask` dispatched through
    /// `McpShellDispatcher` with `peer_job_id = Some(...)` must record the
    /// returned `DecisionId` in the bridge. Without this, `rfc resolve`
    /// could never PTY-inject the answer back to the asking agent.
    #[test]
    fn mcp_ask_with_peer_id_records_into_bridge() {
        use crate::journal::McpDispatcher;
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tmp");
        // Lay out the data_dir as the workspace expects: a project with one
        // pre-created state-machine RFC.
        let project_dir = dir.path().join("projects").join("p");
        std::fs::create_dir_all(project_dir.join("rfcs")).expect("mkdir rfcs");
        // Write a minimal project.toml so Workspace::load picks it up.
        std::fs::write(
            project_dir.join("project.toml"),
            "[project]\nname = \"p\"\n",
        )
        .expect("project.toml");
        let store = orkia_rfc_core::RfcStore::new(project_dir.clone());
        store
            .create(&orkia_rfc_core::RfcId::new("x"), Some("X"))
            .expect("create rfc");

        let bridge = Arc::new(ClarificationPtyBridge::new());
        let router = crate::protocol::EventRouter::new();
        let cache = Arc::new(RfcServiceCache::new());
        let dispatcher = McpShellDispatcher::new(
            dir.path().to_path_buf(),
            router,
            Arc::clone(&bridge),
            cache,
            None,
        );

        // Mimic an MCP-connected agent: send `orkia_rfc_ask` with the
        // listener-supplied peer job id.
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"orkia_rfc_ask","params":{"rfc_id":"x","agent":"faye","question":"iOS?","rationale":"need scope"}}"#;
        let resp = dispatcher.dispatch(req, Some(JobId(42))).response;
        assert!(resp.contains("\"result\""), "expected success: {resp}");

        // Bridge must now hold an entry. We don't know the auto-generated
        // decision id ahead of time, so introspect the response.
        let parsed: serde_json::Value = serde_json::from_str(&resp).expect("parse resp");
        let did_str = parsed["result"]
            .as_str()
            .expect("result is a decision id string");
        let recovered = bridge.take(&orkia_rfc_core::DecisionId::new(did_str));
        assert_eq!(
            recovered,
            Some(JobId(42)),
            "Gap #1: bridge must hold the asking agent's job id after ask"
        );
    }

    /// End-to-end Gap #5 closure: an agent's `orkia_rfc_propose_promote`
    /// must write an `approval.request.json` into the run_dir for the
    /// agent's job_id, with `action="rfc.promote"` and the slug/project
    /// in `metadata.rfc`, so the REPL's approve handler can dispatch the
    /// transition on accept.
    #[test]
    fn mcp_propose_promote_queues_approval_request_file() {
        use crate::journal::McpDispatcher;
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tmp");
        let project_dir = dir.path().join("projects").join("p");
        std::fs::create_dir_all(project_dir.join("rfcs")).expect("mkdir");
        std::fs::write(
            project_dir.join("project.toml"),
            "[project]\nname = \"p\"\n",
        )
        .expect("project.toml");
        let store = orkia_rfc_core::RfcStore::new(project_dir.clone());
        let id = orkia_rfc_core::RfcId::new("x");
        // Drive the RFC into DraftActive so propose_promote is structurally
        // valid (otherwise the dispatcher returns InvalidState and no
        // approval is queued — which is correct: don't queue impossible work).
        let rec = store.create(&id, Some("X")).expect("create");
        let mut fm = rec.fm.clone();
        fm.state = orkia_rfc_core::RfcState::DraftActive;
        store
            .save(fm, String::new())
            .expect("transition to draft-active");

        let bridge = Arc::new(ClarificationPtyBridge::new());
        let router = crate::protocol::EventRouter::new();
        let cache = Arc::new(RfcServiceCache::new());
        let dispatcher = McpShellDispatcher::new(
            dir.path().to_path_buf(),
            router,
            Arc::clone(&bridge),
            cache,
            None,
        );

        let req = r#"{"jsonrpc":"2.0","id":1,"method":"orkia_rfc_propose_promote","params":{"rfc_id":"x","agent":"faye","rationale":"acceptance criteria met"}}"#;
        let resp = dispatcher.dispatch(req, Some(JobId(7))).response;
        assert!(resp.contains("\"queued\":true"), "expected ack: {resp}");

        // The file must have been written into run/<job_id>/.
        let approval_path = dir
            .path()
            .join("run")
            .join("7")
            .join("approval.request.json");
        assert!(
            approval_path.exists(),
            "approval.request.json must exist at {approval_path:?}"
        );
        let body = std::fs::read_to_string(&approval_path).expect("read");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(parsed["action"], "rfc.promote");
        assert_eq!(parsed["metadata"]["rfc"]["slug"], "x");
        assert_eq!(parsed["metadata"]["rfc"]["project"], "p");
        assert_eq!(parsed["metadata"]["rfc"]["agent"], "faye");
    }

    #[test]
    fn resolution_payload_is_newline_terminated() {
        let did = DecisionId::new("d-001");
        let payload = ClarificationPtyBridge::format_resolution(&did, "both platforms");
        let s = std::str::from_utf8(&payload).expect("utf8");
        assert!(s.ends_with('\n'));
        assert!(s.contains("d-001"));
        assert!(s.contains("both platforms"));
    }

    #[tokio::test]
    async fn knowledge_activity_enriches_recall_and_tracks_accessed_nodes() {
        use crate::journal::McpDispatcher;
        use orkia_shell_types::EventType;
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tmp");
        let node_id = seed_domain_node(dir.path(), "auth", "Auth uses PKCE", 0.92);
        let activity = crate::knowledge_activity::spawn_activity_actor();
        let mut env = crate::journal::JournalEnvelope::now(EventType::Hook);
        env.job_id = Some(42);
        env.target = Some("src/auth/pkce.rs".into());
        activity.observe(env);

        let dispatcher = McpShellDispatcher::new(
            dir.path().to_path_buf(),
            crate::protocol::EventRouter::new(),
            Arc::new(ClarificationPtyBridge::new()),
            Arc::new(RfcServiceCache::new()),
            Some(activity),
        );
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"recall","params":{"query":"refactor"}}"#;
        let reply = dispatcher.dispatch(req, Some(JobId(42)));
        let parsed: serde_json::Value = serde_json::from_str(&reply.response).expect("json");
        let ctx = &parsed["result"]["relevant_context"];
        assert_eq!(ctx.as_array().expect("array").len(), 1);
        assert_eq!(ctx[0]["domain"], "auth");
        assert_eq!(ctx[0]["id"], node_id.to_string());
        assert!(reply.accessed_node_ids.contains(&node_id.to_string()));
    }

    #[tokio::test]
    async fn knowledge_activity_skips_context_already_returned_by_recall() {
        use crate::journal::McpDispatcher;
        use orkia_shell_types::EventType;
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tmp");
        let node_id = seed_domain_node(dir.path(), "auth", "Auth uses PKCE", 0.92);
        let activity = crate::knowledge_activity::spawn_activity_actor();
        let mut env = crate::journal::JournalEnvelope::now(EventType::Hook);
        env.job_id = Some(42);
        env.target = Some("src/auth/pkce.rs".into());
        activity.observe(env);

        let dispatcher = McpShellDispatcher::new(
            dir.path().to_path_buf(),
            crate::protocol::EventRouter::new(),
            Arc::new(ClarificationPtyBridge::new()),
            Arc::new(RfcServiceCache::new()),
            Some(activity),
        );
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"recall","params":{"query":"pkce"}}"#;
        let reply = dispatcher.dispatch(req, Some(JobId(42)));
        let parsed: serde_json::Value = serde_json::from_str(&reply.response).expect("json");
        assert!(parsed["result"].get("relevant_context").is_none());
        assert_eq!(reply.accessed_node_ids, vec![node_id.to_string()]);
    }

    #[tokio::test]
    async fn knowledge_activity_enriches_rfc_tool_response() {
        use crate::journal::McpDispatcher;
        use orkia_shell_types::EventType;
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tmp");
        seed_rfc(dir.path(), "x");
        let node_id = seed_domain_node(dir.path(), "auth", "Auth uses PKCE", 0.92);
        let activity = crate::knowledge_activity::spawn_activity_actor();
        let mut env = crate::journal::JournalEnvelope::now(EventType::Hook);
        env.job_id = Some(7);
        env.target = Some("src/auth/pkce.rs".into());
        activity.observe(env);

        let dispatcher = McpShellDispatcher::new(
            dir.path().to_path_buf(),
            crate::protocol::EventRouter::new(),
            Arc::new(ClarificationPtyBridge::new()),
            Arc::new(RfcServiceCache::new()),
            Some(activity),
        );
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"orkia_rfc_state","params":{"rfc_id":"x"}}"#;
        let reply = dispatcher.dispatch(req, Some(JobId(7)));
        let parsed: serde_json::Value = serde_json::from_str(&reply.response).expect("json");
        assert_eq!(parsed["result"]["relevant_context"][0]["domain"], "auth");
        assert!(reply.accessed_node_ids.contains(&node_id.to_string()));
    }

    #[tokio::test]
    async fn knowledge_activity_does_not_enrich_error_response() {
        use crate::journal::McpDispatcher;
        use orkia_shell_types::EventType;
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tmp");
        seed_domain_node(dir.path(), "auth", "Auth uses PKCE", 0.92);
        let activity = crate::knowledge_activity::spawn_activity_actor();
        let mut env = crate::journal::JournalEnvelope::now(EventType::Hook);
        env.job_id = Some(7);
        env.target = Some("src/auth/pkce.rs".into());
        activity.observe(env);

        let dispatcher = McpShellDispatcher::new(
            dir.path().to_path_buf(),
            crate::protocol::EventRouter::new(),
            Arc::new(ClarificationPtyBridge::new()),
            Arc::new(RfcServiceCache::new()),
            Some(activity),
        );
        let reply = dispatcher.dispatch("}{ not json", Some(JobId(7)));
        let parsed: serde_json::Value = serde_json::from_str(&reply.response).expect("json");
        assert!(parsed.get("error").is_some());
        assert!(reply.accessed_node_ids.is_empty());
    }

    fn seed_rfc(data_dir: &std::path::Path, slug: &str) {
        let project_dir = data_dir.join("projects").join("p");
        std::fs::create_dir_all(project_dir.join("rfcs")).expect("mkdir");
        std::fs::write(
            project_dir.join("project.toml"),
            "[project]\nname = \"p\"\n",
        )
        .expect("project.toml");
        let store = orkia_rfc_core::RfcStore::new(project_dir);
        store
            .create(&orkia_rfc_core::RfcId::new(slug), Some("X"))
            .expect("create rfc");
    }

    fn seed_domain_node(
        data_dir: &std::path::Path,
        domain: &str,
        summary: &str,
        confidence: f32,
    ) -> uuid::Uuid {
        use chrono::Utc;
        use orkia_reasoning_core::dto::{KnowledgeNode, RfcRef};
        use orkia_reasoning_core::enums::{KnowledgeNodeKind, NodeOrigin};
        use orkia_reasoning_store::{NodeInsert, ReasoningStore};

        let store_path = crate::reasoning_builtins::store_path(data_dir);
        std::fs::create_dir_all(store_path.parent().expect("reasoning parent"))
            .expect("mkdir reasoning");
        let store = ReasoningStore::open(&store_path).expect("reasoning store");
        let node = KnowledgeNode {
            id: uuid::Uuid::new_v4(),
            workspace_id: uuid::Uuid::from_u128(1),
            project_id: Some(uuid::Uuid::from_u128(3)),
            rfc_ref: Some(RfcRef::new(orkia_rfc_core::RfcId::new("x"))),
            kind: KnowledgeNodeKind::Decision,
            summary: summary.into(),
            confidence,
            origin: NodeOrigin::Cloud,
            created_at: Utc::now(),
        };
        let id = node.id;
        store
            .upsert_node(&NodeInsert {
                node: &node,
                details: None,
                domain: Some(domain),
                context_block: None,
                source_turn_id: None,
                source_session_id: None,
                seal_id: None,
            })
            .expect("upsert node");
        id
    }
}

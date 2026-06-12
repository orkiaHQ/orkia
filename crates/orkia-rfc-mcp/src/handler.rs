// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

use serde::{Deserialize, Serialize};

use orkia_rfc_core::{AgentId, ContentHash, DecisionId, RfcError, RfcId, SectionPath};
use orkia_rfc_state::{AskRequest, EditRequest, LogDecisionRequest, RfcStateService};

use crate::rpc::{
    INVALID_PARAMS, JsonRpcRequest, JsonRpcResponse, METHOD_NOT_FOUND, PARSE_ERROR, RFC_ERROR,
};

/// Top-level dispatch. Returns a JSON-RPC response which the transport layer
/// serializes back to the agent. Notifications (no `id`) still produce a
/// response object that the transport may discard.
pub fn dispatch_request(svc: &RfcStateService, raw: &str) -> JsonRpcResponse {
    // Trust-boundary cap — the caller is an agent process. The
    // transport-layer reader is expected to bound the read separately;
    // this is a defence-in-depth check at the parser.
    if let Err(e) = orkia_shell_types::input_limits::check_len(
        raw.as_bytes(),
        orkia_shell_types::input_limits::MCP_FRAME_MAX_BYTES,
        "orkia-rfc-mcp",
    ) {
        return JsonRpcResponse::err(None, PARSE_ERROR, format!("input rejected: {e}"), None);
    }
    let req: JsonRpcRequest = match serde_json::from_str(raw) {
        Ok(r) => r,
        Err(e) => {
            return JsonRpcResponse::err(None, PARSE_ERROR, format!("parse error: {e}"), None);
        }
    };
    let id = req.id.clone();
    match req.method.as_str() {
        "orkia_rfc_get_context" => handle(svc, id, req.params, get_context),
        "orkia_rfc_state" => handle(svc, id, req.params, get_state),
        "orkia_rfc_list_decisions" => handle(svc, id, req.params, list_decisions),
        "orkia_rfc_ask" => handle(svc, id, req.params, ask),
        "orkia_rfc_log_decision" => handle(svc, id, req.params, log_decision),
        "orkia_rfc_propose_edit" => handle(svc, id, req.params, propose_edit),
        "orkia_rfc_propose_promote" => handle(svc, id, req.params, propose_promote),
        other => JsonRpcResponse::err(
            id,
            METHOD_NOT_FOUND,
            format!("unknown method: {other}"),
            None,
        ),
    }
}

fn handle<P, R, F>(
    svc: &RfcStateService,
    id: Option<serde_json::Value>,
    params: serde_json::Value,
    f: F,
) -> JsonRpcResponse
where
    P: for<'de> Deserialize<'de>,
    R: Serialize,
    F: FnOnce(&RfcStateService, P) -> Result<R, RfcError>,
{
    let parsed: P = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            return JsonRpcResponse::err(id, INVALID_PARAMS, format!("invalid params: {e}"), None);
        }
    };
    match f(svc, parsed) {
        Ok(r) => {
            let v = match serde_json::to_value(&r) {
                Ok(v) => v,
                Err(e) => {
                    return JsonRpcResponse::err(
                        id,
                        crate::rpc::INTERNAL_ERROR,
                        format!("serialize: {e}"),
                        None,
                    );
                }
            };
            JsonRpcResponse::ok(id, v)
        }
        Err(e) => {
            let data = serde_json::to_value(&e).ok();
            JsonRpcResponse::err(id, RFC_ERROR, e.to_string(), data)
        }
    }
}

// ── Params ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GetContextParams {
    rfc_id: RfcId,
    #[serde(default)]
    if_hash_differs_from: Option<ContentHash>,
}

#[derive(Debug, Deserialize)]
struct IdOnlyParams {
    rfc_id: RfcId,
}

#[derive(Debug, Deserialize)]
struct AskParams {
    rfc_id: RfcId,
    agent: AgentId,
    question: String,
    rationale: String,
}

#[derive(Debug, Deserialize)]
struct LogDecisionParams {
    rfc_id: RfcId,
    agent: AgentId,
    content: String,
    rationale: String,
    #[serde(default)]
    affects: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ProposeEditParams {
    rfc_id: RfcId,
    agent: AgentId,
    section: SectionPath,
    new_body: String,
    #[serde(default)]
    linked_decisions: Vec<DecisionId>,
    #[serde(default)]
    if_hash_matches: Option<ContentHash>,
}

#[derive(Debug, Deserialize)]
struct PromoteParams {
    rfc_id: RfcId,
    agent: AgentId,
    rationale: String,
}

// ── Handlers ───────────────────────────────────────────────────────────

fn get_context(
    svc: &RfcStateService,
    p: GetContextParams,
) -> Result<Option<orkia_rfc_state::RfcContext>, RfcError> {
    let ctx = svc.get_context(&p.rfc_id)?;
    if let Some(claimed) = p.if_hash_differs_from {
        if claimed == ctx.content_hash {
            return Ok(None);
        }
    }
    Ok(Some(ctx))
}

#[derive(Serialize)]
struct StateInfo {
    rfc_id: RfcId,
    state: orkia_rfc_core::RfcState,
    version: u32,
    content_hash: ContentHash,
    locked_by: Option<AgentId>,
    open_clarifications: u32,
    unreviewed_decisions: u32,
}

fn get_state(svc: &RfcStateService, p: IdOnlyParams) -> Result<StateInfo, RfcError> {
    let c = svc.get_context(&p.rfc_id)?;
    Ok(StateInfo {
        rfc_id: c.rfc_id,
        state: c.state,
        version: c.version,
        content_hash: c.content_hash,
        locked_by: c.locked_by,
        open_clarifications: c.open_clarifications,
        unreviewed_decisions: c.unreviewed_decisions,
    })
}

fn list_decisions(
    svc: &RfcStateService,
    p: IdOnlyParams,
) -> Result<Vec<orkia_rfc_core::DecisionRecord>, RfcError> {
    svc.store().read_decisions(&p.rfc_id)
}

fn ask(svc: &RfcStateService, p: AskParams) -> Result<DecisionId, RfcError> {
    svc.ask(AskRequest {
        rfc_id: p.rfc_id,
        agent: p.agent,
        question: p.question,
        rationale: p.rationale,
    })
}

fn log_decision(svc: &RfcStateService, p: LogDecisionParams) -> Result<DecisionId, RfcError> {
    svc.log_decision(LogDecisionRequest {
        rfc_id: p.rfc_id,
        agent: p.agent,
        content: p.content,
        rationale: p.rationale,
        affects: p.affects,
    })
}

fn propose_edit(svc: &RfcStateService, p: ProposeEditParams) -> Result<ContentHash, RfcError> {
    svc.propose_edit(EditRequest {
        rfc_id: p.rfc_id,
        agent: p.agent,
        section: p.section,
        new_body: p.new_body,
        linked_decisions: p.linked_decisions,
        if_hash_matches: p.if_hash_matches,
    })
}

#[derive(Serialize)]
struct PromoteAck {
    queued: bool,
    rationale: String,
    rfc_id: RfcId,
    agent: AgentId,
}

/// `orkia_rfc_propose_promote` is **fire-and-forget**: the actual transition
/// is human-gated through the V3 approval flow. This handler validates that
/// the RFC is in a promotable state and that all decisions are reviewed; on
/// success it returns an ack that the shell layer uses to enqueue an
/// approval request. The agent reacts to the approval outcome via PTY
/// injection on its next prompt, not via an async MCP push.
fn propose_promote(svc: &RfcStateService, p: PromoteParams) -> Result<PromoteAck, RfcError> {
    if p.rationale.trim().is_empty() {
        return Err(RfcError::RationaleRequired {
            operation: "propose_promote",
        });
    }
    let ctx = svc.get_context(&p.rfc_id)?;
    use orkia_rfc_core::{RfcState, Transition, TransitionCtx, validate_transition};
    let tctx = TransitionCtx {
        open_clarifications: ctx.open_clarifications,
        unreviewed_decisions: ctx.unreviewed_decisions,
        dispatch_done: true,
    };
    if ctx.state != RfcState::DraftActive {
        return Err(RfcError::InvalidState {
            rfc_id: p.rfc_id,
            state: ctx.state,
            operation: "propose_promote",
            action: "Only DraftActive RFCs may be promoted.".into(),
        });
    }
    validate_transition(&p.rfc_id, ctx.state, Transition::Promote, &tctx)?;
    Ok(PromoteAck {
        queued: true,
        rationale: p.rationale,
        rfc_id: p.rfc_id,
        agent: p.agent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_rfc_core::RfcStore;
    use orkia_rfc_state::{EventSink, RfcEvent};
    use tempfile::tempdir;

    struct DropSink;
    impl EventSink for DropSink {
        fn emit(&self, _: RfcEvent) {}
    }

    fn svc() -> (tempfile::TempDir, RfcStateService) {
        let dir = tempdir().expect("tmpdir");
        let store = RfcStore::new(dir.path().to_path_buf());
        (dir, RfcStateService::new(store, Box::new(DropSink)))
    }

    #[test]
    fn unknown_method() {
        let (_d, s) = svc();
        let resp = dispatch_request(
            &s,
            r#"{"jsonrpc":"2.0","id":1,"method":"orkia_rfc_bogus","params":{}}"#,
        );
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, METHOD_NOT_FOUND);
    }

    #[test]
    fn ask_in_draft_empty_succeeds() {
        let (_d, s) = svc();
        let id = RfcId::new("x");
        s.create(&id, &AgentId::new("h"), None).expect("create");
        let raw = r#"{"jsonrpc":"2.0","id":1,"method":"orkia_rfc_ask","params":{"rfc_id":"x","agent":"faye","question":"q?","rationale":"why"}}"#;
        let resp = dispatch_request(&s, raw);
        assert!(resp.error.is_none(), "ask failed: {:?}", resp.error);
        assert!(resp.result.is_some());
    }

    #[test]
    fn rfc_error_round_trips_with_action_field() {
        let (_d, s) = svc();
        let id = RfcId::new("x");
        s.create(&id, &AgentId::new("h"), None).expect("create");
        // propose_edit in DraftEmpty → InvalidState with educational action.
        let raw = r#"{"jsonrpc":"2.0","id":1,"method":"orkia_rfc_propose_edit","params":{"rfc_id":"x","agent":"faye","section":"Context","new_body":"hi"}}"#;
        let resp = dispatch_request(&s, raw);
        let err = resp.error.expect("error");
        assert_eq!(err.code, RFC_ERROR);
        let data = err.data.expect("data");
        assert_eq!(data["code"], "invalid_state");
        assert!(data["action"].as_str().unwrap().contains("locked"));
    }
}

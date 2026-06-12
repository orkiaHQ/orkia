// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! Consumer transform/ingest tests. These drive `ingest` directly (no tokio
//! task); the real-agent e2e gate (rule #6) lives in `orkia-e2e-harness`.

use super::*;
use orkia_rfc_core::id::RfcId;

fn scope() -> CaptureScope {
    CaptureScope {
        workspace_id: Uuid::from_u128(1),
        account_id: Uuid::from_u128(2),
        project_id: Some(Uuid::from_u128(3)),
        rfc_ref: Some(RfcRef::new(RfcId::new("rfc-9"))),
    }
}

fn hook(event: &str) -> JournalEnvelope {
    JournalEnvelope {
        event_type: EventType::Hook,
        timestamp: "2026-06-05T00:00:00Z".into(),
        job_id: Some(7),
        agent: Some("faye".into()),
        event: Some(event.into()),
        ..Default::default()
    }
}

fn consumer() -> ReasoningConsumer {
    ReasoningConsumer::new(ReasoningStore::in_memory().unwrap(), scope())
}

#[test]
fn non_hook_events_are_ignored() {
    let mut c = consumer();
    let mut env = hook("PreToolUse");
    env.event_type = EventType::Shell;
    assert_eq!(c.ingest(&env).unwrap(), None);
}

#[test]
fn tool_call_turn_lands_with_scope() {
    let mut c = consumer();
    c.ingest(&hook("SessionStart")).unwrap();
    let mut env = hook("PreToolUse");
    env.tool = Some("Bash".into());
    env.target = Some("cargo build".into());
    let id = c.ingest(&env).unwrap().expect("turn written");

    // One session, one turn, scoped to project + rfc.
    let turns = c.store.turns_for_project(Uuid::from_u128(3)).unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].id, id);
    assert_eq!(turns[0].kind, TurnKind::ToolCall("Bash".into()));
    assert_eq!(turns[0].role, TurnRole::Tool);
    let by_rfc = c.store.turns_for_rfc("rfc-9").unwrap();
    assert_eq!(by_rfc.len(), 1);
}

#[test]
fn lazy_session_when_no_session_start() {
    let mut c = consumer();
    // No SessionStart first — consumer joined mid-stream.
    let mut env = hook("UserPromptSubmit");
    env.prompt = Some("hello there".into());
    let id = c.ingest(&env).unwrap().expect("turn written");
    let turns = c.store.turns_for_project(Uuid::from_u128(3)).unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].id, id);
    assert_eq!(turns[0].kind, TurnKind::UserPrompt);
}

#[test]
fn second_turn_links_to_first_as_parent() {
    let mut c = consumer();
    c.ingest(&hook("SessionStart")).unwrap();
    let mut p = hook("UserPromptSubmit");
    p.prompt = Some("do it".into());
    let first = c.ingest(&p).unwrap().unwrap();
    let mut t = hook("PreToolUse");
    t.tool = Some("Read".into());
    c.ingest(&t).unwrap();

    let session_id = c.store.turns_for_project(Uuid::from_u128(3)).unwrap()[0].session_id;
    let turns = c.store.turns_for_session(session_id).unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[1].parent_turn_id, Some(first));
    assert_eq!(turns[1].relation, Some(TurnRelation::FollowUp));
}

#[test]
fn post_tool_use_relation_is_tool_result() {
    let mut c = consumer();
    c.ingest(&hook("SessionStart")).unwrap();
    let mut pre = hook("PreToolUse");
    pre.tool = Some("Bash".into());
    c.ingest(&pre).unwrap();
    let mut post = hook("PostToolUse");
    post.tool = Some("Bash".into());
    post.exit_code = Some(0);
    c.ingest(&post).unwrap();

    let turns = c.store.turns_for_rfc("rfc-9").unwrap();
    let result = turns
        .iter()
        .find(|t| matches!(t.kind, TurnKind::ToolResult(_)))
        .unwrap();
    assert_eq!(result.relation, Some(TurnRelation::ToolResult));
}

#[test]
fn session_end_completes_session() {
    let mut c = consumer();
    c.ingest(&hook("SessionStart")).unwrap();
    let mut env = hook("UserPromptSubmit");
    env.prompt = Some("hi".into());
    c.ingest(&env).unwrap().unwrap();
    let session_id = c.store.turns_for_project(Uuid::from_u128(3)).unwrap()[0].session_id;
    c.ingest(&hook("SessionEnd")).unwrap();
    let s = c.store.get_session(session_id).unwrap().unwrap();
    assert_eq!(
        s.status,
        orkia_reasoning_core::enums::SessionStatus::Completed
    );
}

#[test]
fn unknown_hook_event_is_skipped() {
    let mut c = consumer();
    assert_eq!(c.ingest(&hook("PreCompact")).unwrap(), None);
}

#[test]
fn malformed_envelopes_never_panic_and_capture_defensively() {
    // #7: every byte is untrusted. A hook with a garbage timestamp, no agent,
    // control bytes + a megabyte of text in every field must ingest without
    // panicking and still land a single bounded turn.
    let mut c = consumer();
    let mut env = hook("PreToolUse");
    env.timestamp = "not-a-timestamp\u{0}\u{7f}".into();
    env.agent = None;
    let blob = "A\u{0}\u{7f}🦀 ".repeat(300_000); // ~3 MB across fields
    env.tool = Some(blob.clone());
    env.target = Some(blob.clone());
    env.description = Some(blob);
    let id = c
        .ingest(&env)
        .unwrap()
        .expect("turn written despite garbage");

    let turns = c.store.turns_for_project(Uuid::from_u128(3)).unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].id, id);
    // Summary was scrubbed + capped; the store never held the multi-MB blob.
    assert!(
        turns[0].summary.as_deref().unwrap_or_default().len() <= 1024,
        "summary must be capped"
    );
    // Bad timestamp fell back to a real time (no panic, no zero date).
    assert!(turns[0].occurred_at.timestamp() > 0);
    // Missing agent degraded to the sentinel on the session, not a panic.
    let session = c.store.get_session(turns[0].session_id).unwrap().unwrap();
    assert_eq!(session.agent_name, "unknown");
}

#[test]
fn hook_without_job_id_is_dropped_not_fatal() {
    let mut c = consumer();
    let mut env = hook("PreToolUse");
    env.job_id = None;
    assert_eq!(c.ingest(&env).unwrap(), None);
}

#[test]
fn per_job_scope_overrides_session_scope() {
    // Session-level fallback is project 3 / rfc-9 (see `scope()`); the live
    // per-job entry for job 7 points at a different project + rfc and must win.
    let job_project = Uuid::from_u128(42);
    let scopes = new_job_scopes();
    scopes.write().unwrap().insert(
        7,
        JobScope {
            project_id: Some(job_project),
            rfc_ref: Some(RfcRef::new(RfcId::new("rfc-live"))),
        },
    );
    let mut c =
        ReasoningConsumer::with_job_scopes(ReasoningStore::in_memory().unwrap(), scope(), scopes);

    c.ingest(&hook("SessionStart")).unwrap();
    let mut env = hook("PreToolUse");
    env.tool = Some("Bash".into());
    let id = c.ingest(&env).unwrap().expect("turn written");

    // Lands under the per-job project + rfc, not the session-level fallback.
    let live = c.store.turns_for_project(job_project).unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, id);
    assert_eq!(c.store.turns_for_rfc("rfc-live").unwrap().len(), 1);

    // The session-level fallback captured nothing for this job.
    assert!(
        c.store
            .turns_for_project(Uuid::from_u128(3))
            .unwrap()
            .is_empty()
    );
    assert!(c.store.turns_for_rfc("rfc-9").unwrap().is_empty());
}

#[test]
fn knowledge_access_event_bumps_served_nodes_and_skips_garbage_ids() {
    use orkia_reasoning_core::dto::KnowledgeNode;
    use orkia_reasoning_core::enums::{KnowledgeNodeKind, NodeOrigin};
    use orkia_reasoning_store::NodeInsert;

    let mut c = consumer();
    let id = Uuid::from_u128(42);
    let node = KnowledgeNode {
        id,
        workspace_id: Uuid::from_u128(1),
        project_id: Some(Uuid::from_u128(3)),
        rfc_ref: None,
        kind: KnowledgeNodeKind::Decision,
        summary: "use sqlite".into(),
        confidence: 0.9,
        origin: NodeOrigin::Local,
        created_at: Utc::now(),
    };
    c.store
        .upsert_node(&NodeInsert {
            node: &node,
            details: None,
            domain: None,
            context_block: None,
            source_turn_id: None,
            source_session_id: None,
            seal_id: None,
        })
        .unwrap();
    assert_eq!(c.store.access_count(id).unwrap(), Some(0));

    // A served-read event carrying the real id plus an unparseable one: the real
    // node bumps, the garbage id is skipped (never a panic — CLAUDE.md #7).
    let env = JournalEnvelope::knowledge_access(Some(7), &[id.to_string(), "not-a-uuid".into()]);
    assert_eq!(c.ingest(&env).unwrap(), None);
    assert_eq!(c.store.access_count(id).unwrap(), Some(1));

    // No turn was written — KnowledgeAccess is a decay signal, not a turn.
    assert!(
        c.store
            .turns_for_project(Uuid::from_u128(3))
            .unwrap()
            .is_empty()
    );
}

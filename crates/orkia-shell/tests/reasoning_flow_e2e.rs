// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! End-to-end *flow* of the reasoning graph data plane — the path no single
//! unit test crosses today: a session's journal hooks become turns in the
//! local store (hot path), a simulated cloud pull lands consolidated knowledge
//! nodes + preferences, the `$reasoning` builtins surface them to a human, the
//! enrich step folds preferences + context into an agent's system prompt, and a
//! bulk transcript replay stages dirty turns the way `orkia reasoning backfill`
//! does.
//!
//! This is the data plane, not the PTY plane: capture rides the same
//! `JournalEnvelope` wire type and the same `ReasoningConsumer` the live shell
//! drains off the broadcast bus, driven synchronously here (as backfill does)
//! so the flow is deterministic and needs no real agent, network, or premium
//! credentials. The PTY/attach acceptance lives in the demos harness; this
//! proves the bytes-to-knowledge pipeline end to end.

use std::path::Path;

use chrono::Utc;
use uuid::Uuid;

use orkia_kernel::{
    CaptureScope, JobScope, ReasoningConsumer, enrich_system_prompt, new_job_scopes,
};
use orkia_reasoning_core::PreferenceCache;
use orkia_reasoning_core::dto::{KnowledgeNode, PreferenceDto, RfcRef};
use orkia_reasoning_core::enums::{
    ConversationPhase, Dimension, KnowledgeNodeKind, NodeOrigin, PreferenceScope, SessionStatus,
    TurnKind, TurnRole,
};
use orkia_reasoning_core::phase::{KnowledgeNodeSummary, ReasoningContext, UserPreferenceSummary};
use orkia_reasoning_store::{NodeInsert, PrefUpsert, ReasoningStore};
use orkia_rfc_core::id::RfcId;
use orkia_shell::reasoning_builtins;
use orkia_shell_types::BlockContent;
use orkia_shell_types::journal::{EventType, JournalEnvelope};

const WS: u128 = 0xA11CE;
const ACC: u128 = 0xB0B;
const PROJECT: u128 = 0xC0FFEE;
const JOB: u32 = 7;
const RFC: &str = "RFC-REASONING-1";

// ── shared fixtures ──────────────────────────────────────────────────────────

/// The `<data_dir>/reasoning/reasoning.db` the consumer writes and the builtins
/// read — the exact layout the REPL boots against.
fn store_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("reasoning").join("reasoning.db")
}

fn capture_scope() -> CaptureScope {
    CaptureScope {
        workspace_id: Uuid::from_u128(WS),
        account_id: Uuid::from_u128(ACC),
        project_id: None,
        rfc_ref: None,
    }
}

/// A consumer whose per-job scope attributes `JOB` to a known project + RFC,
/// exactly as the REPL writes at agent spawn. The consumer owns the single
/// store writer (CLAUDE.md #2).
fn consumer_for(store: ReasoningStore) -> ReasoningConsumer {
    let job_scopes = new_job_scopes();
    job_scopes.write().unwrap().insert(
        JOB,
        JobScope {
            project_id: Some(Uuid::from_u128(PROJECT)),
            rfc_ref: Some(RfcRef::new(RfcId::new(RFC))),
        },
    );
    ReasoningConsumer::with_job_scopes(store, capture_scope(), job_scopes)
}

/// A journal `Hook` envelope for `event` attributed to `JOB`.
fn hook(event: &str) -> JournalEnvelope {
    let mut env = JournalEnvelope::now(EventType::Hook);
    env.job_id = Some(JOB);
    env.agent = Some("faye".into());
    env.event = Some(event.into());
    env
}

/// Render any builtin's block output to a single string for assertions.
fn text_of(blocks: &[BlockContent]) -> String {
    blocks
        .iter()
        .map(|b| match b {
            BlockContent::SystemInfo(s) | BlockContent::Text(s) => s.clone(),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Seed a consolidated cloud node (what `SyncWorker::pull` upserts), with the
/// provenance fields the KG carries.
fn seed_node(store: &ReasoningStore, kind: KnowledgeNodeKind, summary: &str, domain: &str) {
    let node = KnowledgeNode {
        id: Uuid::new_v4(),
        workspace_id: Uuid::from_u128(WS),
        project_id: Some(Uuid::from_u128(PROJECT)),
        rfc_ref: Some(RfcRef::new(RfcId::new(RFC))),
        kind,
        summary: summary.into(),
        confidence: 0.88,
        origin: NodeOrigin::Cloud,
        created_at: Utc::now(),
    };
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
        .unwrap();
}

// ── 1. hot path: journal hooks → turns + session lifecycle ───────────────────

#[test]
fn capture_flow_lands_a_full_session_with_lifecycle_and_dirty_bookkeeping() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut consumer = consumer_for(ReasoningStore::open(&path).unwrap());

    // A realistic agent turn sequence on one job, in order.
    consumer.ingest(&hook("SessionStart")).unwrap();
    let mut prompt = hook("UserPromptSubmit");
    prompt.prompt = Some("review the auth module".into());
    consumer.ingest(&prompt).unwrap();
    let mut pre = hook("PreToolUse");
    pre.tool = Some("Bash".into());
    pre.target = Some("cargo test".into());
    consumer.ingest(&pre).unwrap();
    let mut post = hook("PostToolUse");
    post.tool = Some("Bash".into());
    post.response_preview = Some("ok: 42 passed".into());
    consumer.ingest(&post).unwrap();
    let mut stop = hook("Stop");
    stop.response_preview = Some("the auth module looks correct".into());
    consumer.ingest(&stop).unwrap();
    consumer.ingest(&hook("SessionEnd")).unwrap();

    // Read back through a fresh read-only connection (WAL: reader alongside the
    // writer), as the builtins do.
    let store = ReasoningStore::open(&path).unwrap();

    // Four turns landed (SessionStart/End are lifecycle, not turns), attributed
    // to the per-job project and RFC, in sequence order.
    let turns = store.turns_for_project(Uuid::from_u128(PROJECT)).unwrap();
    assert_eq!(turns.len(), 4, "prompt + 2 tool turns + agent output");
    let kinds: Vec<TurnKind> = turns.iter().map(|t| t.kind.clone()).collect();
    assert_eq!(
        kinds,
        vec![
            TurnKind::UserPrompt,
            TurnKind::ToolCall("Bash".into()),
            TurnKind::ToolResult("Bash".into()),
            TurnKind::AgentOutput,
        ]
    );
    assert!(
        turns
            .iter()
            .all(|t| t.project_id == Some(Uuid::from_u128(PROJECT)))
    );
    assert_eq!(turns[0].role, TurnRole::User);
    // The consumer stamps seq starting at 1 (JobTrack starts at 0, incremented
    // before each turn), monotonic across the session.
    assert!(
        turns.iter().map(|t| t.seq).eq(1i64..5),
        "seq is monotonic from 1"
    );

    // Same turns are reachable by RFC scope (the graph/recall query path).
    assert_eq!(store.turns_for_rfc(RFC).unwrap().len(), 4);

    // The session was opened then completed by SessionEnd.
    let stats = store.stats().unwrap();
    assert_eq!(stats.sessions, 1);
    assert_eq!(stats.turns, 4);
    let session_id = turns[0].session_id;
    assert_eq!(
        store.get_session(session_id).unwrap().unwrap().status,
        SessionStatus::Completed,
    );

    // Every captured turn is dirty — awaiting the cold-path sync. Nothing was
    // synced (no backend), so the bookkeeping is intact.
    assert_eq!(stats.dirty_turns, 4, "all turns pending sync");
    assert_eq!(store.dirty_turn_dtos(100).unwrap().len(), 4);
}

#[test]
fn capture_is_defensive_a_malformed_or_lifeless_envelope_never_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut consumer = consumer_for(ReasoningStore::open(&path).unwrap());

    // A hook with no `event` field, a non-hook event, and a hook with an unknown
    // event name all classify to nothing — no turn, no panic (#7).
    let mut no_event = JournalEnvelope::now(EventType::Hook);
    no_event.job_id = Some(JOB);
    assert_eq!(consumer.ingest(&no_event).unwrap(), None);
    assert_eq!(consumer.ingest(&hook("NotARealHook")).unwrap(), None);

    // A turn-shaped hook with no job_id cannot be attributed → skipped.
    let mut orphan = hook("Stop");
    orphan.job_id = None;
    assert_eq!(consumer.ingest(&orphan).unwrap(), None);

    let store = ReasoningStore::open(&path).unwrap();
    assert_eq!(store.stats().unwrap().turns, 0);
}

// ── 2. cloud pull → human-facing `$reasoning` builtins ───────────────────────

#[test]
fn graph_recall_and_status_builtins_surface_synced_knowledge() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path();
    let path = store_path(data_dir);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();

    // Simulate a cold-path pull: consolidated nodes land in the local store.
    {
        let store = ReasoningStore::open(&path).unwrap();
        seed_node(
            &store,
            KnowledgeNodeKind::Decision,
            "Auth uses PKCE for the device flow",
            "security",
        );
        seed_node(
            &store,
            KnowledgeNodeKind::Constraint,
            "Never log bearer tokens",
            "security",
        );
    }

    // `$reasoning graph` lists every node; the RFC and domain filters scope it.
    let all = text_of(&reasoning_builtins::graph(data_dir, &[]));
    assert!(all.contains("knowledge graph (2 nodes)"), "got: {all}");
    assert!(all.contains("PKCE"));

    let by_rfc = text_of(&reasoning_builtins::graph(
        data_dir,
        &["--rfc".into(), RFC.into()],
    ));
    assert!(by_rfc.contains("2 nodes"), "RFC scope: {by_rfc}");

    let by_domain = text_of(&reasoning_builtins::graph(
        data_dir,
        &["--domain".into(), "security".into()],
    ));
    assert!(by_domain.contains("2 nodes"), "domain scope: {by_domain}");

    // `$reasoning recall` is the human mirror of the agent's MCP recall: it
    // renders the deterministic context block for a summary match.
    let recall = text_of(&reasoning_builtins::recall(data_dir, &["pkce".into()]));
    assert!(recall.contains("recall"));
    assert!(
        recall.contains("[DECISION:0.88]"),
        "context block: {recall}"
    );
    assert!(
        text_of(&reasoning_builtins::recall(
            data_dir,
            &["nonexistent".into()]
        ))
        .contains("no cached"),
        "a miss is explicit, never a panic"
    );

    // `$reasoning status` with no live Intelligence reports the inactive,
    // premium-required state (the store counts are only appended when a live
    // Intelligence is present — the no-login path returns before stats).
    let status = text_of(&reasoning_builtins::status(None, data_dir));
    assert!(status.contains("inactive"), "status: {status}");
    assert!(status.contains("premium feature"), "status: {status}");
}

#[test]
fn graph_builtin_fails_closed_on_an_empty_or_missing_store() {
    let tmp = tempfile::tempdir().unwrap();
    // No store file at all → a clean message, never an error or panic.
    let missing = text_of(&reasoning_builtins::graph(tmp.path(), &[]));
    assert!(missing.contains("no reasoning data yet"), "got: {missing}");

    // A bad flag is reported, not actioned.
    let path = store_path(tmp.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    ReasoningStore::open(&path).unwrap();
    let bad = text_of(&reasoning_builtins::graph(tmp.path(), &["--bogus".into()]));
    assert!(bad.contains("unknown flag"), "got: {bad}");
}

// ── 3. enrich: preferences + context → agent system prompt ───────────────────

#[test]
fn enrich_folds_synced_preferences_and_context_into_the_system_prompt() {
    let ws = Uuid::from_u128(WS);

    // A cold-path pull warms the preference cache (what SyncWorker::pull does).
    let cache = PreferenceCache::new();
    cache.put(
        ws,
        vec![
            PreferenceDto {
                dimension: Dimension::Verbosity,
                value: "concise".into(),
                confidence: 0.92,
                observation_count: 5,
                scope: PreferenceScope::Workspace,
            },
            // Below the 0.6 confidence floor — must NOT be injected.
            PreferenceDto {
                dimension: Dimension::Tone,
                value: "formal".into(),
                confidence: 0.3,
                observation_count: 1,
                scope: PreferenceScope::Workspace,
            },
        ],
    );

    let ctx = ReasoningContext {
        classified_turns: vec![],
        relevant_knowledge: vec![KnowledgeNodeSummary {
            kind: KnowledgeNodeKind::Decision,
            summary: "use sqlite for the local store".into(),
            confidence: 0.8,
        }],
        user_preferences: vec![UserPreferenceSummary {
            dimension: Dimension::Verbosity,
            value: "concise".into(),
            confidence: 0.92,
        }],
        inflexion_points: vec!["chose WAL over a write queue".into()],
        conversation_phase: ConversationPhase::NearingDecision,
    };

    let out = enrich_system_prompt("You are Orkia.", &cache, ws, Some(&ctx));

    // High-confidence preference injected; low-confidence one suppressed.
    assert!(out.contains("<user_preferences>"));
    assert!(out.contains("verbosity: concise"));
    assert!(
        !out.contains("formal"),
        "sub-threshold pref must be dropped"
    );

    // Reasoning context block appended with phase + known decisions.
    assert!(out.contains("--- REASONING CONTEXT ---"));
    assert!(out.contains("nearing-decision"));
    assert!(out.contains("[decision] use sqlite for the local store"));

    // Determinism (KV-cache stability): same inputs → byte-identical output.
    let again = enrich_system_prompt("You are Orkia.", &cache, ws, Some(&ctx));
    assert_eq!(out, again);
}

#[test]
fn enrich_is_a_passthrough_when_there_is_nothing_to_add() {
    let cache = PreferenceCache::new();
    let out = enrich_system_prompt("base prompt", &cache, Uuid::from_u128(WS), None);
    assert_eq!(out, "base prompt", "no prefs, no context → untouched");
}

// ── 4. backfill: bulk transcript replay → staged dirty turns ─────────────────

/// Mirror `reasoning_backfill::stage`: replay JSONL `JournalEnvelope` lines
/// through the real consumer (synchronous, no drops), skipping malformed lines.
fn replay_jsonl(path: &Path, scope: CaptureScope, jsonl: &str) -> usize {
    let store = ReasoningStore::open(path).unwrap();
    let mut consumer = ReasoningConsumer::with_job_scopes(store, scope, new_job_scopes());
    let mut turns = 0usize;
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Untrusted input (#7): a malformed line is skipped, never fatal.
        let Ok(env) = serde_json::from_str::<JournalEnvelope>(line) else {
            continue;
        };
        if let Ok(Some(_)) = consumer.ingest(&env) {
            turns += 1;
        }
    }
    turns
}

/// One historical session's worth of envelopes as the Python parser emits them:
/// a synthetic per-session job_id, in order. `event` drives classification.
fn transcript_jsonl(job_id: u32) -> String {
    let env = |event: &str, extra: serde_json::Value| {
        // The wire shape the Python parser emits: `type` (lowercase enum) and
        // `timestamp` are REQUIRED (no serde default on those two fields).
        let mut v = serde_json::json!({
            "type": "hook",
            "timestamp": "2026-01-02T03:04:05Z",
            "job_id": job_id,
            "agent": "faye",
            "event": event,
        });
        if let (Some(obj), Some(ex)) = (v.as_object_mut(), extra.as_object()) {
            for (k, val) in ex {
                obj.insert(k.clone(), val.clone());
            }
        }
        v.to_string()
    };
    [
        env("SessionStart", serde_json::json!({})),
        env(
            "UserPromptSubmit",
            serde_json::json!({ "prompt": "port the parser" }),
        ),
        env(
            "PreToolUse",
            serde_json::json!({ "tool": "Edit", "target": "parser.rs" }),
        ),
        env("Stop", serde_json::json!({ "response_preview": "ported" })),
        env("SessionEnd", serde_json::json!({})),
    ]
    .join("\n")
}

#[test]
fn backfill_replay_stages_dirty_turns_and_skips_garbage() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();

    // Two historical sessions plus a blank line and a corrupt line in the middle.
    let corpus = format!(
        "{}\n   \nnot-json-at-all\n{}",
        transcript_jsonl(101),
        transcript_jsonl(102),
    );

    let staged = replay_jsonl(&path, capture_scope(), &corpus);
    assert_eq!(staged, 6, "3 turns per session, garbage skipped");

    let store = ReasoningStore::open(&path).unwrap();
    let stats = store.stats().unwrap();
    assert_eq!(stats.sessions, 2, "two historical sessions opened");
    assert_eq!(stats.turns, 6);
    // Backfilled turns are dirty → the one-shot `backfill_sync` would push them.
    assert_eq!(stats.dirty_turns, 6);
    assert_eq!(store.dirty_turn_dtos(100).unwrap().len(), 6);
}

/// Characterization (not aspiration): consumer-level replay is NOT idempotent.
/// `build_dto` mints a fresh `client_event_id` (random uuid) for every ingest,
/// and `insert_turn`'s `INSERT OR IGNORE` keys on that id — so replaying the
/// same corpus writes a SECOND, distinct copy of every turn. Re-running
/// `orkia reasoning backfill` on the same transcript therefore double-counts
/// locally, and since the regenerated id is also the cloud idempotency key, a
/// re-push would not dedup either. Documented here so a future change that
/// derives the id from the envelope (making replay safe) flips this assertion
/// deliberately rather than by accident. See FINDINGS in the worktree summary.
#[test]
fn backfill_replay_is_not_idempotent_today_each_run_duplicates_turns() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();

    let one_session = transcript_jsonl(200);
    let first = replay_jsonl(&path, capture_scope(), &one_session);
    let second = replay_jsonl(&path, capture_scope(), &one_session);
    assert_eq!(first, 3);
    assert_eq!(second, 3, "no dedup: a fresh client_event_id per ingest");

    let store = ReasoningStore::open(&path).unwrap();
    assert_eq!(
        store.stats().unwrap().turns,
        6,
        "replay duplicated all three turns",
    );
    // The session row, however, is reused — the second SessionStart lazily
    // reopens the same job track only within a run; across runs a new session is
    // created, so two sessions exist for the one transcript.
    assert_eq!(store.stats().unwrap().sessions, 2);
}

// ── 5. preference upsert round-trip (cold-path write the cache warms from) ────

#[test]
fn preference_upsert_round_trips_through_the_local_store() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let store = ReasoningStore::open(&path).unwrap();

    store
        .upsert_preference(&PrefUpsert {
            workspace_id: Uuid::from_u128(WS),
            account_id: Uuid::from_u128(ACC),
            pref: PreferenceDto {
                dimension: Dimension::Language,
                value: "français".into(),
                confidence: 0.75,
                observation_count: 4,
                scope: PreferenceScope::Workspace,
            },
            scope_id: None,
        })
        .unwrap();

    let prefs = store
        .preferences_for_workspace(Uuid::from_u128(WS))
        .unwrap();
    assert_eq!(prefs.len(), 1);
    assert_eq!(prefs[0].dimension, Dimension::Language);
    assert_eq!(prefs[0].value, "français");
}

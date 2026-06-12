// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use std::path::Path;

use chrono::Utc;
use orkia_reasoning_core::dto::KnowledgeNode;
use orkia_reasoning_core::enums::{KnowledgeNodeKind, NodeOrigin};
use orkia_reasoning_store::NodeInsert;
use orkia_shell_types::{EventType, JournalEnvelope};
use serde_json::Value;
use uuid::Uuid;

use super::*;

#[test]
fn parses_navigation_refs() {
    let id = Uuid::new_v4();
    assert_eq!(
        SourceRef::parse(&format!("kg://node/{id}")),
        Some(SourceRef::KnowledgeNode(id))
    );
    assert_eq!(
        SourceRef::parse("kg:abcdef12"),
        Some(SourceRef::KnowledgePrefix("abcdef12".into()))
    );
    assert_eq!(
        SourceRef::parse("journal://event/7"),
        Some(SourceRef::Journal(7))
    );
    assert_eq!(SourceRef::parse("seal:9"), Some(SourceRef::Journal(9)));
    assert_eq!(
        SourceRef::parse("agent:@sage"),
        Some(SourceRef::Trail(TrailRef {
            kind: TrailKind::Agent,
            value: "sage".into()
        }))
    );
    assert_eq!(
        SourceRef::parse("seal:seal-auth-1"),
        Some(SourceRef::Trail(TrailRef {
            kind: TrailKind::Seal,
            value: "seal-auth-1".into()
        }))
    );
}

#[test]
fn resolves_knowledge_node_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let id = seed_node(dir.path(), "Auth uses PKCE");
    let prefix = &id.to_string()[..8];
    let journal = JournalStore::new(dir.path());
    let blocks = resolve(dir.path(), &journal, &format!("kg:{prefix}"));
    let text = blocks
        .into_iter()
        .map(|block| format!("{block:?}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Auth uses PKCE"), "{text}");
}

#[test]
fn resolves_global_journal_index() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = JournalStore::new(dir.path());
    let mut first = JournalEnvelope::now(EventType::Hook);
    first.event = Some("agent.spawn".into());
    journal.append(&first);
    let mut second = JournalEnvelope::now(EventType::Hook);
    second.event = Some("operator.projection_answered".into());
    second.message = Some("auth projection".into());
    journal.append(&second);
    let blocks = resolve(dir.path(), &journal, "journal://event/2");
    let text = blocks
        .into_iter()
        .map(|block| format!("{block:?}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("operator.projection_answered"), "{text}");
    assert!(text.contains("auth projection"), "{text}");
}

#[test]
fn resolves_journal_json_with_source_trail() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = JournalStore::new(dir.path());
    let mut env = JournalEnvelope::now(EventType::Hook);
    env.event = Some("operator.projection_answered".into());
    env.agent = Some("sage".into());
    env.job_id = Some(42);
    env.extra.insert(
        "seal_id".into(),
        serde_json::Value::String("seal-auth-1".into()),
    );
    env.extra
        .insert("rfc_id".into(), serde_json::Value::String("auth-v2".into()));
    journal.append(&env);

    let value = resolve_json(dir.path(), &journal, "journal://event/1");
    assert_eq!(value.get("found").and_then(Value::as_bool), Some(true));
    let trail = value.get("trail").and_then(Value::as_array).expect("trail");
    assert!(trail.iter().any(|item| {
        item.get("kind").and_then(Value::as_str) == Some("job")
            && item.get("source_ref").and_then(Value::as_str) == Some("job:42")
    }));
    assert!(trail.iter().any(|item| {
        item.get("kind").and_then(Value::as_str) == Some("seal")
            && item.get("source_ref").and_then(Value::as_str) == Some("seal:seal-auth-1")
    }));
}

#[test]
fn resolves_trail_ref_to_related_events() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = JournalStore::new(dir.path());
    let mut env = JournalEnvelope::now(EventType::Hook);
    env.event = Some("operator.projection_answered".into());
    env.message = Some("auth projection".into());
    env.agent = Some("sage".into());
    journal.append(&env);

    let value = resolve_json(dir.path(), &journal, "agent:@sage");
    assert_eq!(value.get("found").and_then(Value::as_bool), Some(true));
    assert_eq!(value.get("kind").and_then(Value::as_str), Some("agent"));
    let events = value
        .get("events")
        .and_then(Value::as_array)
        .expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].get("source_ref").and_then(Value::as_str),
        Some("journal://event/1")
    );
}

fn seed_node(data_dir: &Path, summary: &str) -> Uuid {
    let path = crate::reasoning_builtins::store_path(data_dir);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let store = ReasoningStore::open(&path).unwrap();
    let node = KnowledgeNode {
        id: Uuid::new_v4(),
        workspace_id: Uuid::from_u128(1),
        project_id: None,
        rfc_ref: None,
        kind: KnowledgeNodeKind::Decision,
        summary: summary.into(),
        confidence: 0.9,
        origin: NodeOrigin::Cloud,
        created_at: Utc::now(),
    };
    store
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
    node.id
}

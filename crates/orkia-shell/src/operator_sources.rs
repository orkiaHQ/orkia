// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use std::path::Path;

use orkia_reasoning_core::compile_context_block;
use orkia_reasoning_store::{KnowledgeNodeSearchHit, ReasoningStore};
use orkia_shell_types::BlockContent;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::journal::{JournalFilter, JournalStore};

pub fn resolve(data_dir: &Path, journal: &JournalStore, raw: &str) -> Vec<BlockContent> {
    render_result(resolve_json(data_dir, journal, raw))
}

pub fn resolve_json(data_dir: &Path, journal: &JournalStore, raw: &str) -> Value {
    match SourceRef::parse(raw) {
        Some(SourceRef::KnowledgeNode(id)) => resolve_node(data_dir, raw, &id),
        Some(SourceRef::KnowledgePrefix(prefix)) => resolve_node_prefix(data_dir, raw, &prefix),
        Some(SourceRef::Journal(index)) => resolve_journal(journal, raw, index),
        Some(SourceRef::Trail(trail)) => resolve_trail(journal, raw, &trail),
        None => error_json(raw, "unsupported source reference"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SourceRef {
    KnowledgeNode(Uuid),
    KnowledgePrefix(String),
    Journal(usize),
    Trail(TrailRef),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrailRef {
    kind: TrailKind,
    value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrailKind {
    Agent,
    Job,
    Rfc,
    Seal,
    Session,
    Turn,
}

impl SourceRef {
    fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        parse_kg_uri(trimmed)
            .or_else(|| parse_short_kg(trimmed))
            .or_else(|| parse_journal(trimmed))
            .or_else(|| parse_trail(trimmed))
    }
}

fn resolve_node(data_dir: &Path, raw: &str, id: &Uuid) -> Value {
    let Some(store) = open_store(data_dir) else {
        return error_json(raw, "no reasoning store");
    };
    match store.node_hit_by_id(*id) {
        Ok(Some(hit)) => node_json(raw, hit),
        Ok(None) => error_json(raw, &format!("no node `{id}`")),
        Err(err) => error_json(raw, &err.to_string()),
    }
}

fn resolve_node_prefix(data_dir: &Path, raw: &str, prefix: &str) -> Value {
    let Some(store) = open_store(data_dir) else {
        return error_json(raw, "no reasoning store");
    };
    let Ok(matches) = store.node_hits_by_prefix(prefix, 20) else {
        return error_json(raw, "failed to read reasoning store");
    };
    match matches.as_slice() {
        [] => error_json(raw, &format!("no node matching `{prefix}`")),
        [hit] => node_json(raw, hit.clone()),
        _ => json!({
            "reference": raw,
            "kind": "knowledge_node",
            "found": false,
            "error": format!("`{prefix}` is ambiguous"),
            "matches": matches.iter().map(|hit| hit.node.id.to_string()).collect::<Vec<_>>(),
        }),
    }
}

fn resolve_journal(journal: &JournalStore, raw: &str, index: usize) -> Value {
    let filter = JournalFilter::default();
    let Some((_, env)) = journal
        .query_indexed(&filter)
        .into_iter()
        .find(|(idx, _)| *idx == index)
    else {
        return error_json(raw, &format!("no journal event `{index}`"));
    };
    let seal = env
        .extra
        .get("seal_id")
        .or_else(|| env.extra.get("seal_path"))
        .and_then(|value| value.as_str());
    json!({
        "reference": raw,
        "kind": "journal_event",
        "found": true,
        "source_ref": format!("journal://event/{index}"),
        "event": {
            "index": index,
            "timestamp": env.timestamp,
            "source": env.source,
            "event": env.event,
            "job_id": env.job_id,
            "agent": env.agent,
            "message": env.message,
            "extra": env.extra,
        },
        "trail": journal_trail_json(env.job_id, env.agent.as_deref(), seal, env.extra.get("rfc_id").or_else(|| env.extra.get("rfc")).and_then(|value| value.as_str())),
    })
}

fn resolve_trail(journal: &JournalStore, raw: &str, trail: &TrailRef) -> Value {
    let filter = JournalFilter::default();
    let events = journal
        .query_indexed(&filter)
        .into_iter()
        .filter(|(_, env)| trail_matches(trail, env))
        .take(20)
        .map(|(index, env)| event_summary(index, env))
        .collect::<Vec<_>>();
    json!({
        "reference": raw,
        "kind": trail.kind.label(),
        "found": true,
        "source_ref": raw,
        "target": trail.value,
        "events": events,
        "trail": [trail_json(trail.kind, trail.value.as_str())],
    })
}

fn node_json(raw: &str, hit: KnowledgeNodeSearchHit) -> Value {
    let node = hit.node;
    let id = node.id.to_string();
    let source_ref = if let Some(rfc) = &node.rfc_ref {
        format!("kg://rfc/{}/node/{id}", rfc.rfc_id)
    } else {
        format!("kg://node/{id}")
    };
    let rfc = node.rfc_ref.as_ref().map(|r| r.rfc_id.to_string());
    json!({
        "reference": raw,
        "kind": "knowledge_node",
        "found": true,
        "source_ref": source_ref,
        "node": {
            "id": id,
            "type": format!("{:?}", node.kind),
            "summary": node.summary,
            "confidence": node.confidence,
            "created_at": node.created_at.to_rfc3339(),
            "rfc": rfc,
            "domain": hit.domain,
            "context": hit.context_block.unwrap_or_else(|| compile_context_block(&node)),
        },
        "trail": node_trail_json(hit.agent_name.as_deref(), hit.source_session_id, hit.source_turn_id, hit.seal_id.as_deref(), rfc.as_deref()),
        "seal_id": hit.seal_id,
        "agent": hit.agent_name,
        "source_session_id": hit.source_session_id.map(|id| id.to_string()),
        "source_turn_id": hit.source_turn_id.map(|id| id.to_string()),
    })
}

fn render_result(value: Value) -> Vec<BlockContent> {
    if value.get("found").and_then(Value::as_bool) != Some(true) {
        let msg = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("source not found");
        return vec![BlockContent::Error(format!("operator open: {msg}"))];
    }
    let source_ref = value
        .get("source_ref")
        .and_then(Value::as_str)
        .unwrap_or("-");
    let mut blocks = vec![BlockContent::SystemInfo(format!(" {source_ref}"))];
    if let Some(node) = value.get("node") {
        push_object_fields(
            &mut blocks,
            node,
            &[
                "id",
                "type",
                "confidence",
                "created_at",
                "rfc",
                "domain",
                "summary",
                "context",
            ],
        );
    }
    if let Some(event) = value.get("event") {
        push_object_fields(
            &mut blocks,
            event,
            &[
                "index",
                "timestamp",
                "source",
                "event",
                "job_id",
                "agent",
                "message",
                "extra",
            ],
        );
    }
    if let Some(target) = value.get("target").and_then(Value::as_str) {
        blocks.push(BlockContent::Text(format!("  target: {target}")));
    }
    if let Some(events) = value.get("events").and_then(Value::as_array) {
        blocks.push(BlockContent::Text(format!(
            "  related events: {}",
            events.len()
        )));
        for event in events.iter().take(20) {
            let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
            let name = event.get("event").and_then(Value::as_str).unwrap_or("-");
            let message = event.get("message").and_then(Value::as_str).unwrap_or("-");
            blocks.push(BlockContent::Text(format!(
                "    journal://event/{index} {name} {message}"
            )));
        }
    }
    if let Some(trail) = value.get("trail").and_then(Value::as_array)
        && !trail.is_empty()
    {
        blocks.push(BlockContent::Text("  source trail:".into()));
        for item in trail {
            if let Some(line) = trail_line(item) {
                blocks.push(BlockContent::Text(format!("    {line}")));
            }
        }
    }
    blocks
}

fn event_summary(index: usize, env: &crate::journal::JournalEnvelope) -> Value {
    json!({
        "index": index,
        "source_ref": format!("journal://event/{index}"),
        "timestamp": env.timestamp,
        "source": env.source,
        "event": env.event,
        "job_id": env.job_id,
        "agent": env.agent,
        "message": env.message,
    })
}

fn trail_matches(trail: &TrailRef, env: &crate::journal::JournalEnvelope) -> bool {
    match trail.kind {
        TrailKind::Agent => env.agent.as_deref() == Some(trail.value.as_str()),
        TrailKind::Job => trail
            .value
            .parse::<u32>()
            .ok()
            .is_some_and(|job| env.job_id == Some(job)),
        TrailKind::Rfc => extra_matches(env, &["rfc_id", "rfc"], &trail.value),
        TrailKind::Seal => extra_matches(env, &["seal_id", "seal_path"], &trail.value),
        TrailKind::Session => {
            extra_matches(env, &["session_id", "source_session_id"], &trail.value)
        }
        TrailKind::Turn => extra_matches(env, &["turn_id", "source_turn_id"], &trail.value),
    }
}

fn extra_matches(env: &crate::journal::JournalEnvelope, keys: &[&str], expected: &str) -> bool {
    keys.iter().any(|key| {
        env.extra
            .get(*key)
            .and_then(Value::as_str)
            .is_some_and(|value| value == expected)
    })
}

fn trail_line(item: &Value) -> Option<String> {
    if let Some(text) = item.as_str() {
        return Some(text.to_string());
    }
    let kind = item.get("kind").and_then(Value::as_str)?;
    let label = item.get("label").and_then(Value::as_str).unwrap_or("-");
    let source_ref = item
        .get("source_ref")
        .and_then(Value::as_str)
        .map(|value| format!(" ref={value}"))
        .unwrap_or_default();
    Some(format!("{kind} {label}{source_ref}"))
}

fn push_object_fields(blocks: &mut Vec<BlockContent>, object: &Value, keys: &[&str]) {
    for key in keys {
        let Some(value) = object.get(*key) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        let text = value
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| value.to_string());
        blocks.push(BlockContent::Text(format!("  {key}: {text}")));
    }
}

fn node_trail_json(
    agent: Option<&str>,
    session: Option<Uuid>,
    turn: Option<Uuid>,
    seal: Option<&str>,
    rfc: Option<&str>,
) -> Vec<Value> {
    let mut trail = Vec::new();
    if let Some(agent) = agent {
        trail.push(trail_json(TrailKind::Agent, agent));
    }
    if let Some(session) = session {
        trail.push(trail_json(TrailKind::Session, &session.to_string()));
    }
    if let Some(turn) = turn {
        trail.push(trail_json(TrailKind::Turn, &turn.to_string()));
    }
    if let Some(rfc) = rfc {
        trail.push(trail_json(TrailKind::Rfc, rfc));
    }
    if let Some(seal) = seal {
        trail.push(trail_json(TrailKind::Seal, seal));
    }
    trail
}

fn journal_trail_json(
    job_id: Option<u32>,
    agent: Option<&str>,
    seal: Option<&str>,
    rfc: Option<&str>,
) -> Vec<Value> {
    let mut trail = Vec::new();
    if let Some(job_id) = job_id {
        trail.push(trail_json(TrailKind::Job, &job_id.to_string()));
    }
    if let Some(agent) = agent {
        trail.push(trail_json(TrailKind::Agent, agent));
    }
    if let Some(rfc) = rfc {
        trail.push(trail_json(TrailKind::Rfc, rfc));
    }
    if let Some(seal) = seal {
        trail.push(trail_json(TrailKind::Seal, seal));
    }
    trail
}

fn trail_json(kind: TrailKind, value: &str) -> Value {
    let label = match kind {
        TrailKind::Agent => format!("@{value}"),
        _ => value.to_string(),
    };
    json!({
        "kind": kind.label(),
        "label": label,
        "source_ref": format!("{}:{value}", kind.label()),
    })
}

impl TrailKind {
    fn label(self) -> &'static str {
        match self {
            TrailKind::Agent => "agent",
            TrailKind::Job => "job",
            TrailKind::Rfc => "rfc",
            TrailKind::Seal => "seal",
            TrailKind::Session => "session",
            TrailKind::Turn => "turn",
        }
    }
}

fn error_json(raw: &str, error: &str) -> Value {
    json!({
        "reference": raw,
        "found": false,
        "error": error,
    })
}

fn open_store(data_dir: &Path) -> Option<ReasoningStore> {
    let path = crate::reasoning_builtins::store_path(data_dir);
    ReasoningStore::open(&path).ok()
}

fn parse_kg_uri(raw: &str) -> Option<SourceRef> {
    let id = raw
        .strip_prefix("kg://node/")
        .or_else(|| raw.split_once("/node/").map(|(_, id)| id))?;
    id.parse().ok().map(SourceRef::KnowledgeNode)
}

fn parse_short_kg(raw: &str) -> Option<SourceRef> {
    let prefix = raw.strip_prefix("kg:")?;
    is_hex_prefix(prefix).then(|| SourceRef::KnowledgePrefix(prefix.to_string()))
}

fn parse_journal(raw: &str) -> Option<SourceRef> {
    let index = raw
        .strip_prefix("journal://event/")
        .or_else(|| raw.strip_prefix("journal:"))
        .or_else(|| {
            raw.strip_prefix("seal:")
                .filter(|value| value.chars().all(|ch| ch.is_ascii_digit()))
        })?
        .parse()
        .ok()?;
    (index > 0).then_some(SourceRef::Journal(index))
}

fn parse_trail(raw: &str) -> Option<SourceRef> {
    let (kind, value) = raw.split_once(':')?;
    let kind = match kind {
        "agent" => TrailKind::Agent,
        "job" => TrailKind::Job,
        "rfc" => TrailKind::Rfc,
        "seal" => TrailKind::Seal,
        "session" => TrailKind::Session,
        "turn" => TrailKind::Turn,
        _ => return None,
    };
    let value = value.trim_start_matches('@');
    (!value.is_empty()).then(|| {
        SourceRef::Trail(TrailRef {
            kind,
            value: value.to_string(),
        })
    })
}

fn is_hex_prefix(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests;

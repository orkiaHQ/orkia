// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Wire translation: local types → backend NDJSON envelopes.
//!
//! The structs mirror the backend's sync wire contract
//! by **wire**, not by dependency. The orkia/ workspace is public
//! (Elastic-2.0); the proprietary distribution is private. We pay the cost of
//! a small parallel definition to keep the dependency line clean.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Serialize to a JSON value, logging (not swallowing) a failure before
/// falling back to `Null` so a malformed `data:null` frame on the wire is at
/// least traceable (BUG-100).
fn json_or_null<T: Serialize>(value: T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "orkia-stream: payload serialization failed; emitting null");
        serde_json::Value::Null
    })
}

use orkia_shell_types::journal::{EventType, JournalEnvelope};
use orkia_shell_types::seal::SealRecord;

/// One NDJSON line on the wire — `{"entity_type":"...","data":{...}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushLine {
    pub entity_type: String,
    pub data: serde_json::Value,
}

impl PushLine {
    pub fn serialized_size(&self) -> usize {
        serde_json::to_vec(self).map(|v| v.len()).unwrap_or(0)
    }
}

/// Wire form of a local seal-record push.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalSealRecordPush {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub account_id: Uuid,
    pub team_id: Option<Uuid>,
    pub chain_id: String,
    pub seq: i64,
    pub parent_hash: Option<String>,
    pub content_hash: String,
    pub content: serde_json::Value,
}

/// Wire form of a journal-event push.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEventPush {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub account_id: Uuid,
    pub team_id: Option<Uuid>,
    pub seq: i64,
    pub event_type: String,
    pub source: Option<String>,
    pub agent_name: Option<String>,
    pub job_id: Option<i32>,
    pub event: Option<String>,
    pub tool: Option<String>,
    pub target: Option<String>,
    pub exit_code: Option<i32>,
    pub action: Option<String>,
    pub risk: Option<String>,
    pub description: Option<String>,
    pub message: Option<String>,
    pub payload: serde_json::Value,
    pub timestamp: DateTime<Utc>,
    pub scope: String,
}

/// Wire form of a public-job sync push
/// context; the server re-confirms project scope at write time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicJobPush {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub project_name: String,
    pub agent_name: String,
    pub model: String,
    pub status: String,
    pub task_summary: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub runtime_ms: Option<i64>,
    pub exit_code: Option<i32>,
}

/// Wire form of a public-routing-decision sync push.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicRoutingDecisionPush {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub project_name: String,
    pub intent: String,
    pub target_agent_name: String,
    pub target_model: String,
    pub confidence: f32,
    pub trust_at_routing: f32,
    pub job_id: Option<Uuid>,
    pub routed_at: DateTime<Utc>,
}

fn extra_str(env: &JournalEnvelope, key: &str) -> Option<String> {
    env.extra
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn extra_time(env: &JournalEnvelope, key: &str) -> Option<DateTime<Utc>> {
    env.extra
        .get(key)
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}

/// Build a `public_job` push line from a `job.spawned` / `job.complete`
/// journal envelope. Returns `None` (skip) when the required `id`/`project`
/// fields are absent.
pub fn to_public_job_push(env: &JournalEnvelope, workspace_id: Uuid) -> Option<PushLine> {
    let id = Uuid::parse_str(&extra_str(env, "id")?).ok()?;
    let push = PublicJobPush {
        id,
        workspace_id,
        project_name: extra_str(env, "project")?,
        agent_name: extra_str(env, "agent_name").unwrap_or_default(),
        model: extra_str(env, "model").unwrap_or_default(),
        status: extra_str(env, "status").unwrap_or_else(|| "running".into()),
        task_summary: extra_str(env, "task_summary").unwrap_or_default(),
        started_at: extra_time(env, "started_at").unwrap_or_else(Utc::now),
        completed_at: extra_time(env, "completed_at"),
        runtime_ms: env.extra.get("runtime_ms").and_then(|v| v.as_i64()),
        exit_code: env
            .extra
            .get("exit_code")
            .and_then(|v| v.as_i64())
            .map(|n| n as i32),
    };
    Some(PushLine {
        entity_type: "public_job".to_string(),
        data: json_or_null(push),
    })
}

/// Build a `public_routing_decision` push line from a `routing.decided`
/// journal envelope.
pub fn to_public_routing_push(env: &JournalEnvelope, workspace_id: Uuid) -> Option<PushLine> {
    let id = Uuid::parse_str(&extra_str(env, "id")?).ok()?;
    let push = PublicRoutingDecisionPush {
        id,
        workspace_id,
        project_name: extra_str(env, "project")?,
        intent: extra_str(env, "intent").unwrap_or_default(),
        target_agent_name: extra_str(env, "target_agent_name").unwrap_or_default(),
        target_model: extra_str(env, "target_model").unwrap_or_default(),
        confidence: env
            .extra
            .get("confidence")
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0) as f32,
        trust_at_routing: env
            .extra
            .get("trust_at_routing")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0) as f32,
        job_id: extra_str(env, "job_id").and_then(|s| Uuid::parse_str(&s).ok()),
        routed_at: extra_time(env, "routed_at").unwrap_or_else(Utc::now),
    };
    Some(PushLine {
        entity_type: "public_routing_decision".to_string(),
        data: json_or_null(push),
    })
}

/// Build a `local_seal_record` push line.
///
/// The translator still threads it through for the day team support
/// arrives.
pub fn to_local_seal_push(
    chain_id: &str,
    record: &SealRecord,
    workspace_id: Uuid,
    account_id: Uuid,
    team_id: Option<Uuid>,
    scope_label: &str,
) -> PushLine {
    // content so the backend's seal-merge can lift it into the
    // dedicated `seal_record.rfc_id` column. Absent for records that
    // weren't emitted under an RFC scope.
    let mut content = serde_json::json!({
        "event_type": record.event_type,
        "timestamp": record.timestamp,
        "detail": record.detail,
        "scope": scope_label,
    });
    if let Some(rfc_id) = &record.rfc_id
        && let Some(obj) = content.as_object_mut()
    {
        obj.insert(
            "rfc_id".to_string(),
            serde_json::Value::String(rfc_id.as_str().to_string()),
        );
    }
    let push = LocalSealRecordPush {
        id: Uuid::now_v7(),
        workspace_id,
        account_id,
        team_id,
        chain_id: chain_id.to_string(),
        seq: record.seq as i64,
        parent_hash: Some(record.prev_hash.clone()),
        content_hash: record.hash.clone(),
        content,
    };
    PushLine {
        entity_type: "local_seal_record".to_string(),
        data: json_or_null(push),
    }
}

/// Build a `journal_event` push line.
pub fn to_journal_push(
    env: &JournalEnvelope,
    workspace_id: Uuid,
    account_id: Uuid,
    team_id: Option<Uuid>,
    scope_label: &str,
) -> PushLine {
    let ts = chrono::DateTime::parse_from_rfc3339(&env.timestamp)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
    let event_type = match env.event_type {
        EventType::Hook => "hook",
        EventType::Approval => "approval",
        EventType::Lifecycle => "lifecycle",
        EventType::Shell => "shell",
        EventType::Tell => "tell",
        EventType::Seal => "seal",
        EventType::ScopeChange => "scope_change",
        EventType::KnowledgeAccess => "knowledge_access",
    };
    // Build a payload that captures every field of the envelope so the
    // backend has the full event available (the typed columns are an
    // index; `payload` is the source of truth).
    let payload = json_or_null(env);

    let seq = env.extra.get("seq").and_then(|v| v.as_i64()).unwrap_or(0);

    let push = JournalEventPush {
        id: Uuid::now_v7(),
        workspace_id,
        account_id,
        team_id,
        seq,
        event_type: event_type.to_string(),
        source: env.source.clone(),
        agent_name: env.agent.clone(),
        job_id: env.job_id.map(|j| j as i32),
        event: env.event.clone(),
        tool: env.tool.clone(),
        target: env.target.clone(),
        exit_code: env.exit_code,
        action: env.action.clone(),
        risk: env.risk.clone(),
        description: env.description.clone(),
        message: env.message.clone(),
        payload,
        timestamp: ts,
        scope: scope_label.to_string(),
    };
    PushLine {
        entity_type: "journal_event".to_string(),
        data: json_or_null(push),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> SealRecord {
        SealRecord {
            seq: 7,
            timestamp: "2026-05-26T00:00:00+00:00".into(),
            event_type: "rfc.create".into(),
            detail: serde_json::json!({"scope": "public", "rfc": "abc"}),
            hash: "h".into(),
            prev_hash: "p".into(),
            rfc_id: None,
        }
    }

    #[test]
    fn seal_push_has_expected_shape() {
        let ws = Uuid::new_v4();
        let acc = Uuid::new_v4();
        let line = to_local_seal_push("workspace", &record(), ws, acc, None, "public");
        assert_eq!(line.entity_type, "local_seal_record");
        let data = line.data.as_object().unwrap();
        assert_eq!(data["chain_id"], "workspace");
        assert_eq!(data["seq"], 7);
        assert_eq!(data["content_hash"], "h");
        assert_eq!(data["parent_hash"], "p");
        assert_eq!(data["content"]["scope"], "public");
        assert_eq!(data["team_id"], serde_json::Value::Null);
    }

    #[test]
    fn journal_push_has_expected_shape() {
        let ws = Uuid::new_v4();
        let acc = Uuid::new_v4();
        let mut env = JournalEnvelope::now(EventType::Hook);
        env.event = Some("PreToolUse".into());
        env.agent = Some("faye".into());
        env.job_id = Some(42);
        env.tool = Some("Read".into());
        env.extra
            .insert("scope".into(), serde_json::Value::String("public".into()));
        let line = to_journal_push(&env, ws, acc, None, "public");
        assert_eq!(line.entity_type, "journal_event");
        let data = line.data.as_object().unwrap();
        assert_eq!(data["event_type"], "hook");
        assert_eq!(data["event"], "PreToolUse");
        assert_eq!(data["agent_name"], "faye");
        assert_eq!(data["scope"], "public");
    }

    #[test]
    fn public_job_push_maps_extra_fields() {
        let ws = Uuid::new_v4();
        let job_uuid = Uuid::new_v4();
        let mut env = JournalEnvelope::now(EventType::Lifecycle);
        env.event = Some("job.spawned".into());
        for (k, v) in [
            ("id", serde_json::json!(job_uuid.to_string())),
            ("project", serde_json::json!("orkia-shell")),
            ("scope", serde_json::json!("public")),
            ("agent_name", serde_json::json!("faye")),
            ("model", serde_json::json!("sonnet-4.6")),
            ("task_summary", serde_json::json!("refactor SealChain")),
            ("status", serde_json::json!("running")),
            ("started_at", serde_json::json!("2026-05-26T00:00:00+00:00")),
        ] {
            env.extra.insert(k.into(), v);
        }
        let line = to_public_job_push(&env, ws).expect("push built");
        assert_eq!(line.entity_type, "public_job");
        let d = line.data.as_object().unwrap();
        // Field names must match the server-side PublicJobPush by wire.
        assert_eq!(d["id"], job_uuid.to_string());
        assert_eq!(d["workspace_id"], ws.to_string());
        assert_eq!(d["project_name"], "orkia-shell");
        assert_eq!(d["agent_name"], "faye");
        assert_eq!(d["model"], "sonnet-4.6");
        assert_eq!(d["task_summary"], "refactor SealChain");
        assert_eq!(d["status"], "running");
    }

    #[test]
    fn public_job_push_requires_id_and_project() {
        let ws = Uuid::new_v4();
        let mut env = JournalEnvelope::now(EventType::Lifecycle);
        env.event = Some("job.spawned".into());
        // No `id`/`project` extras → no push (skip rather than emit garbage).
        assert!(to_public_job_push(&env, ws).is_none());
    }
}

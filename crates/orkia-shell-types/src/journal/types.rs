// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Journal event envelope.
//!
//! A single flat, permissive type carries every event in the orkia
//! ecosystem — agent hooks, approvals, job lifecycle, shell SEAL, tells.
//! It travels over the Unix socket as NDJSON and is also emitted
//! in-process for shell-originated events. The journal indexes on
//! `event_type`, `job_id`, `agent`, and `timestamp`; everything else
//! is payload, including provider-specific fields captured in `extra`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EventType {
    #[default]
    Hook,
    Approval,
    Lifecycle,
    Shell,
    Tell,
    Seal,
    /// A scope was set or changed on an artifact (workspace, project, RFC, issue).
    /// Emitted after the corresponding SEAL record is durably persisted.
    /// PR1a defines the variant; PR1b begins emitting it.
    ScopeChange,
    /// A premium agent read knowledge-graph nodes via the `orkia-knowledge-mcp`
    /// (`node_ids`); the REPL-owned reasoning consumer applies the access bump.
    /// Not SEAL-chained (SEAL records are emitted deliberately, not off the bus).
    KnowledgeAccess,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct JournalEnvelope {
    #[serde(rename = "type")]
    pub event_type: EventType,
    pub timestamp: String,

    /// Monotonic sequence stamped by the daemon-resident journal hub at its
    /// re-subscribing REPL receive the durable backlog journaled while it was
    /// down, so the SEAL chain spans a REPL restart unbroken. `None` on
    /// envelopes that never passed through the daemon hub (single-process
    /// daemon-less fallback, per-job local hub) — those have no resubscribe
    /// gap to close. Named `hub_seq` (not `seq`) to avoid colliding with the
    /// SEAL audit record's own `seq` carried in `extra`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub_seq: Option<u64>,

    // Routing fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,

    // Event-specific fields. Optional and permissive so providers can
    // populate what makes sense for them; missing fields are normal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,

    // Final-response fields. Populated only on AgentFinalResponse
    // envelopes (event_type = Hook, event = "AgentFinalResponse"); all
    // other event kinds leave these None. Owned by
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_preview: Option<String>,

    // Pipeline fields. Populated only on PipelineOutput envelopes
    // (event = "PipelineOutput") and on agent.spawn records in Team
    // mode. Solo never sets these but parses them so OSS
    // tooling can inspect Team-produced journals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage_index: Option<u32>,

    // Catch-all for provider-specific fields we have not modelled.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl JournalEnvelope {
    pub fn now(event_type: EventType) -> Self {
        Self {
            event_type,
            timestamp: chrono::Utc::now().to_rfc3339(),
            ..Self::default()
        }
    }

    /// Build a `KnowledgeAccess` event carrying the ids of nodes a premium agent
    /// them back with [`Self::knowledge_access_ids`] and applies the access bump.
    pub fn knowledge_access(job_id: Option<u32>, node_ids: &[String]) -> Self {
        let mut env = Self::now(EventType::KnowledgeAccess);
        env.job_id = job_id;
        env.event = Some("KnowledgeAccess".into());
        let ids = node_ids
            .iter()
            .cloned()
            .map(serde_json::Value::String)
            .collect();
        env.extra
            .insert("node_ids".into(), serde_json::Value::Array(ids));
        env
    }

    /// The served node ids on a `KnowledgeAccess` event (defensive: any missing
    /// or non-string entry is skipped, never a panic).
    pub fn knowledge_access_ids(&self) -> Vec<String> {
        self.extra
            .get("node_ids")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Filter applied to in-memory `JournalStore` queries and `orkia journal`
/// CLI output. All fields are AND-combined; `None` means "no constraint".
#[derive(Debug, Clone, Default)]
pub struct JournalFilter {
    pub agent: Option<String>,
    pub job_id: Option<u32>,
    pub event_type: Option<EventType>,
    /// Match on the `event` string field (e.g. "Stop",
    /// "AgentFinalResponse"). Independent of `event_type`.
    pub event: Option<String>,
    pub source: Option<String>,
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    pub last_n: Option<usize>,
}

impl JournalFilter {
    pub fn matches(&self, e: &JournalEnvelope) -> bool {
        if let Some(ref agent) = self.agent
            && e.agent.as_deref() != Some(agent.as_str())
        {
            return false;
        }
        if let Some(job_id) = self.job_id
            && e.job_id != Some(job_id)
        {
            return false;
        }
        if let Some(event_type) = self.event_type
            && e.event_type != event_type
        {
            return false;
        }
        if let Some(ref event) = self.event
            && e.event.as_deref() != Some(event.as_str())
        {
            return false;
        }
        if let Some(ref source) = self.source
            && e.source.as_deref() != Some(source.as_str())
        {
            return false;
        }
        if let Some(since) = self.since {
            match chrono::DateTime::parse_from_rfc3339(&e.timestamp) {
                Ok(ts) => {
                    if ts.with_timezone(&chrono::Utc) < since {
                        return false;
                    }
                }
                Err(_) => return false,
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(
        event_type: EventType,
        agent: Option<&str>,
        job_id: Option<u32>,
    ) -> JournalEnvelope {
        JournalEnvelope {
            event_type,
            timestamp: "2026-05-20T10:00:00+00:00".into(),
            agent: agent.map(String::from),
            job_id,
            ..Default::default()
        }
    }

    #[test]
    fn filter_matches_agent() {
        let f = JournalFilter {
            agent: Some("faye".into()),
            ..Default::default()
        };
        assert!(f.matches(&envelope(EventType::Hook, Some("faye"), Some(1))));
        assert!(!f.matches(&envelope(EventType::Hook, Some("killua"), Some(1))));
        assert!(!f.matches(&envelope(EventType::Hook, None, Some(1))));
    }

    #[test]
    fn filter_matches_job_id() {
        let f = JournalFilter {
            job_id: Some(2),
            ..Default::default()
        };
        assert!(f.matches(&envelope(EventType::Hook, Some("faye"), Some(2))));
        assert!(!f.matches(&envelope(EventType::Hook, Some("faye"), Some(3))));
        assert!(!f.matches(&envelope(EventType::Hook, Some("faye"), None)));
    }

    #[test]
    fn filter_matches_event_type() {
        let f = JournalFilter {
            event_type: Some(EventType::Approval),
            ..Default::default()
        };
        assert!(f.matches(&envelope(EventType::Approval, None, None)));
        assert!(!f.matches(&envelope(EventType::Hook, None, None)));
    }

    #[test]
    fn filter_empty_matches_all() {
        let f = JournalFilter::default();
        assert!(f.matches(&envelope(EventType::Hook, Some("faye"), Some(1))));
        assert!(f.matches(&envelope(EventType::Shell, None, None)));
    }

    #[test]
    fn filter_combines_constraints() {
        let f = JournalFilter {
            agent: Some("faye".into()),
            event_type: Some(EventType::Hook),
            ..Default::default()
        };
        assert!(f.matches(&envelope(EventType::Hook, Some("faye"), None)));
        assert!(!f.matches(&envelope(EventType::Shell, Some("faye"), None)));
        assert!(!f.matches(&envelope(EventType::Hook, Some("killua"), None)));
    }

    #[test]
    fn envelope_serde_round_trip() {
        let mut env = JournalEnvelope::now(EventType::Hook);
        env.job_id = Some(1);
        env.agent = Some("faye".into());
        env.event = Some("PreToolUse".into());
        env.tool = Some("Read".into());
        env.target = Some("src/auth/mod.rs".into());
        env.extra.insert("custom".into(), serde_json::json!(42));

        let s = serde_json::to_string(&env).expect("serialize");
        let back: JournalEnvelope = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back.event_type, EventType::Hook);
        assert_eq!(back.job_id, Some(1));
        assert_eq!(back.agent.as_deref(), Some("faye"));
        assert_eq!(back.event.as_deref(), Some("PreToolUse"));
        assert_eq!(back.tool.as_deref(), Some("Read"));
        assert_eq!(back.target.as_deref(), Some("src/auth/mod.rs"));
        assert_eq!(back.extra.get("custom"), Some(&serde_json::json!(42)));
    }

    #[test]
    fn envelope_omits_none_fields() {
        let env = JournalEnvelope::now(EventType::Shell);
        let s = serde_json::to_string(&env).expect("serialize");
        // The "type" field uses serde rename = "type"; tag should be lowercase.
        assert!(s.contains("\"type\":\"shell\""));
        // None fields should not be present.
        assert!(!s.contains("job_id"));
        assert!(!s.contains("tool"));
    }

    #[test]
    fn envelope_deserializes_minimal() {
        let s = r#"{"type":"hook","timestamp":"2026-05-20T10:00:00+00:00"}"#;
        let env: JournalEnvelope = serde_json::from_str(s).expect("deserialize");
        assert_eq!(env.event_type, EventType::Hook);
        assert_eq!(env.timestamp, "2026-05-20T10:00:00+00:00");
        assert!(env.job_id.is_none());
    }

    #[test]
    fn filter_since_excludes_older() {
        let since = chrono::DateTime::parse_from_rfc3339("2026-05-20T11:00:00+00:00")
            .expect("parse since")
            .with_timezone(&chrono::Utc);
        let f = JournalFilter {
            since: Some(since),
            ..Default::default()
        };
        // envelope timestamp is 10:00, before 11:00 — should not match.
        assert!(!f.matches(&envelope(EventType::Hook, None, None)));
        let mut later = envelope(EventType::Hook, None, None);
        later.timestamp = "2026-05-20T12:00:00+00:00".into();
        assert!(f.matches(&later));
    }

    #[test]
    fn filter_matches_event_name() {
        let f = JournalFilter {
            event: Some("AgentFinalResponse".into()),
            ..Default::default()
        };
        let mut e = envelope(EventType::Hook, Some("faye"), Some(1));
        e.event = Some("AgentFinalResponse".into());
        assert!(f.matches(&e));
        let mut other = envelope(EventType::Hook, Some("faye"), Some(1));
        other.event = Some("Stop".into());
        assert!(!f.matches(&other));
        assert!(!f.matches(&envelope(EventType::Hook, Some("faye"), Some(1))));
    }

    #[test]
    fn envelope_response_fields_roundtrip() {
        let mut env = JournalEnvelope::now(EventType::Hook);
        env.event = Some("AgentFinalResponse".into());
        env.job_id = Some(42);
        env.response_path = Some("/tmp/jobs/42/final-response.md".into());
        env.response_sha256 = Some("0123456789abcdef".into());
        env.response_bytes = Some(1247);
        env.response_preview = Some("hello world".into());

        let s = serde_json::to_string(&env).expect("serialize");
        assert!(s.contains("\"response_path\""));
        assert!(s.contains("\"response_sha256\""));
        assert!(s.contains("\"response_bytes\":1247"));

        let back: JournalEnvelope = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back.event.as_deref(), Some("AgentFinalResponse"));
        assert_eq!(back.response_bytes, Some(1247));
        assert_eq!(back.response_sha256.as_deref(), Some("0123456789abcdef"));
    }

    #[test]
    fn envelope_response_fields_omitted_when_none() {
        let env = JournalEnvelope::now(EventType::Hook);
        let s = serde_json::to_string(&env).expect("serialize");
        assert!(!s.contains("response_path"));
        assert!(!s.contains("response_sha256"));
        assert!(!s.contains("response_bytes"));
        assert!(!s.contains("response_preview"));
    }
}

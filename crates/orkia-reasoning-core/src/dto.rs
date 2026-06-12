// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! Wire DTOs for the reasoning graph — the shapes the shell sends to, and
//! receives from, the cloud `/v1/reasoning/*` endpoints. Every closed domain
//! is an enum (see [`crate::enums`]); the serde form here is identical to the
//! one stored in the local SQLite TEXT columns, so there is one vocabulary
//! end to end.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use orkia_rfc_core::id::{RfcId, SectionPath};

use crate::enums::{
    Dimension, KnowledgeNodeKind, NodeOrigin, PreferenceScope, SignalDirection, TurnKind,
    TurnRelation, TurnRole,
};

/// Optional link from a reasoning record to a specific RFC (and, optionally, a
/// section within it). Threaded through every ingest path so knowledge nodes
/// and preferences can be attributed back to the RFC that produced them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RfcRef {
    pub rfc_id: RfcId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<SectionPath>,
}

impl RfcRef {
    pub fn new(rfc_id: RfcId) -> Self {
        Self {
            rfc_id,
            section: None,
        }
    }

    pub fn with_section(rfc_id: RfcId, section: SectionPath) -> Self {
        Self {
            rfc_id,
            section: Some(section),
        }
    }
}

/// A single conversational turn captured by the hot path and synced to cloud.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TurnDto {
    /// Stable id assigned locally at capture; the idempotency key for sync.
    pub client_event_id: Uuid,
    pub session_id: Option<Uuid>,
    pub workspace_id: Uuid,
    /// Optional project scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    /// Optional RFC link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rfc_ref: Option<RfcRef>,
    pub agent_name: String,
    pub role: TurnRole,
    /// What the turn is (user prompt, tool call, …). Replaces the legacy free
    /// `turn_type` string.
    pub kind: TurnKind,
    pub summary: String,
    pub content_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_count: Option<i32>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
    pub parent_turn_id: Option<Uuid>,
    /// The edge/link type to `parent_turn_id`. Replaces the legacy free
    /// `relation_type` string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation: Option<TurnRelation>,
    pub occurred_at: DateTime<Utc>,
}

/// A preference signal observed from a turn (hot path), pushed to cloud for
/// cold-pass consolidation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignalDto {
    pub client_event_id: Uuid,
    pub workspace_id: Uuid,
    pub account_id: Uuid,
    pub source_session_id: Uuid,
    pub source_turn_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rfc_ref: Option<RfcRef>,
    pub dimension: Dimension,
    pub direction: SignalDirection,
    pub strength: f32,
}

/// An effective preference returned by the cloud (post cold-pass), cached
/// locally and injected into agent system prompts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreferenceDto {
    pub dimension: Dimension,
    pub value: String,
    pub confidence: f32,
    pub observation_count: i32,
    pub scope: PreferenceScope,
}

/// A consolidated knowledge node (cold-pass output): a durable fact, decision,
/// discovery, or constraint distilled from many turns.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KnowledgeNode {
    pub id: Uuid,
    pub workspace_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rfc_ref: Option<RfcRef>,
    pub kind: KnowledgeNodeKind,
    pub summary: String,
    pub confidence: f32,
    /// Which engine produced this row. Cloud is authoritative; local-origin
    /// rows are the only ones eligible for upload.
    pub origin: NodeOrigin,
    pub created_at: DateTime<Utc>,
}

/// Compile a node into its injection text — a **pure, deterministic** template
/// over the node's structured fields. No LLM, no I/O, no clock: the same node
/// always yields byte-identical output.
///
/// context block must be byte-stable so the provider can reuse its KV-cache
/// across turns, and because templated context outperforms LLM-rendered prose.
/// The cold pass *extracts* the node (an LLM job); it never writes this text.
/// Either side (cloud at consolidation, shell at read) can call this and get the
/// same bytes, so the wire only needs to carry the structured fields.
pub fn compile_context_block(node: &KnowledgeNode) -> String {
    let mut out = format!(
        "[{}:{:.2}] {}",
        node.kind.as_str().to_uppercase(),
        node.confidence,
        node.summary.trim()
    );
    if let Some(rfc) = node.rfc_ref.as_ref() {
        out.push_str(&format!(" (rfc {})", rfc.rfc_id.as_str()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> Uuid {
        Uuid::from_u128(1)
    }

    fn node(kind: KnowledgeNodeKind, summary: &str, rfc: Option<&str>) -> KnowledgeNode {
        KnowledgeNode {
            id: Uuid::from_u128(9),
            workspace_id: ws(),
            project_id: None,
            rfc_ref: rfc.map(|r| RfcRef::new(RfcId::new(r))),
            kind,
            summary: summary.into(),
            confidence: 0.92,
            origin: NodeOrigin::Cloud,
            created_at: DateTime::from_timestamp(0, 0).unwrap(),
        }
    }

    #[test]
    fn compile_context_block_is_deterministic_and_templated() {
        let n = node(KnowledgeNodeKind::Decision, "Auth uses PKCE", Some("rfc-7"));
        // Pure: identical bytes on every call (KV-cache stability).
        assert_eq!(compile_context_block(&n), compile_context_block(&n));
        assert_eq!(
            compile_context_block(&n),
            "[DECISION:0.92] Auth uses PKCE (rfc rfc-7)"
        );

        // No RFC → no suffix; an `Other` kind renders its raw tag, uppercased.
        let m = node(
            KnowledgeNodeKind::Other("hypothesis".into()),
            "  spaces trimmed  ",
            None,
        );
        assert_eq!(
            compile_context_block(&m),
            "[HYPOTHESIS:0.92] spaces trimmed"
        );
    }

    #[test]
    fn turn_dto_round_trips_with_enum_relation() {
        let dto = TurnDto {
            client_event_id: Uuid::from_u128(7),
            session_id: Some(Uuid::from_u128(2)),
            workspace_id: ws(),
            project_id: None,
            rfc_ref: Some(RfcRef::with_section(
                RfcId::new("rfc-42"),
                SectionPath::new("goals"),
            )),
            agent_name: "faye".into(),
            role: TurnRole::Agent,
            kind: TurnKind::ToolCall("Bash".into()),
            summary: "ran build".into(),
            content_hash: "abc".into(),
            token_count: Some(12),
            metadata: serde_json::Value::Null,
            parent_turn_id: Some(Uuid::from_u128(3)),
            relation: Some(TurnRelation::ToolResult),
            occurred_at: DateTime::from_timestamp(0, 0).unwrap(),
        };
        let json = serde_json::to_string(&dto).unwrap();
        let back: TurnDto = serde_json::from_str(&json).unwrap();
        assert_eq!(back.relation, Some(TurnRelation::ToolResult));
        assert_eq!(back.kind, TurnKind::ToolCall("Bash".into()));
        assert_eq!(back.rfc_ref.unwrap().section.unwrap().as_str(), "goals");
    }

    #[test]
    fn null_metadata_is_omitted_from_wire() {
        let dto = TurnDto {
            client_event_id: Uuid::from_u128(7),
            session_id: None,
            workspace_id: ws(),
            project_id: None,
            rfc_ref: None,
            agent_name: "a".into(),
            role: TurnRole::User,
            kind: TurnKind::UserPrompt,
            summary: "hi".into(),
            content_hash: "h".into(),
            token_count: None,
            metadata: serde_json::Value::Null,
            parent_turn_id: None,
            relation: None,
            occurred_at: DateTime::from_timestamp(0, 0).unwrap(),
        };
        let json = serde_json::to_string(&dto).unwrap();
        assert!(!json.contains("metadata"));
        assert!(!json.contains("rfc_ref"));
        assert!(!json.contains("relation"));
    }

    #[test]
    fn preference_dto_uses_plain_string_dimension() {
        let p = PreferenceDto {
            dimension: Dimension::Verbosity,
            value: "concise".into(),
            confidence: 0.9,
            observation_count: 3,
            scope: PreferenceScope::Workspace,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"dimension\":\"verbosity\""));
        let back: PreferenceDto = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }
}

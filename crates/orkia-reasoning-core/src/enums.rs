// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! Closed graph domains as enums — never strings.
//!
//! Every value here has ONE serde representation, used verbatim by the JSON
//! wire (HTTP sync) and by the SQLite/Postgres TEXT columns, so the shell and
//! the backend share one vocabulary. Renaming a variant is a wire/schema
//! migration. At the trust boundary, unknown variants fail closed for the
//! closed enums (deserialize errors → caller skips the record); the two
//! deliberately open domains, [`Dimension`] and [`KnowledgeNodeKind`], instead
//! round-trip unknown values losslessly via their `Other` variant.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Who produced a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnRole {
    User,
    Agent,
    System,
    Tool,
}

/// The graph EDGE/LINK type between two turns. This was a free string
/// (`relation_type`) in the legacy graph — now a closed enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnRelation {
    /// `target` is the conversational parent of `source`.
    Parent,
    /// A follow-up turn in the same thread.
    FollowUp,
    /// A direct reply.
    Reply,
    /// A retry of a previous attempt.
    Retry,
    /// The tool result that answers a tool call.
    ToolResult,
    /// A correction of a previous turn.
    Correction,
}

/// What a turn *is*. The discriminant is closed; the open part (a tool's name)
/// stays a field rather than being baked into the discriminant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "tool", rename_all = "snake_case")]
pub enum TurnKind {
    UserPrompt,
    ToolCall(String),
    ToolResult(String),
    AgentOutput,
    SessionStart,
    SessionEnd,
}

impl TurnKind {
    /// Stable discriminant for indexing/columns (the tool name, if any, is
    /// stored separately — see `orkia-reasoning-store`).
    pub fn discriminant(&self) -> &'static str {
        match self {
            TurnKind::UserPrompt => "user_prompt",
            TurnKind::ToolCall(_) => "tool_call",
            TurnKind::ToolResult(_) => "tool_result",
            TurnKind::AgentOutput => "agent_output",
            TurnKind::SessionStart => "session_start",
            TurnKind::SessionEnd => "session_end",
        }
    }

    /// The tool name carried by tool-call/tool-result kinds, if any.
    pub fn tool(&self) -> Option<&str> {
        match self {
            TurnKind::ToolCall(t) | TurnKind::ToolResult(t) => Some(t.as_str()),
            _ => None,
        }
    }
}

/// Kind of consolidated knowledge node produced by the cold pass.
///
/// This is the one closed-graph domain that is deliberately *open*, like
/// [`Dimension`]: the local shell and the cloud backend historically grew
/// slightly different taxonomies — the shell minted `Fact`, the cloud minted
/// `Artifact` — and a forward-version cloud may add more. The union of both
/// vocabularies is the wire contract, and any value outside it round-trips
/// losslessly through [`KnowledgeNodeKind::Other`] instead of erroring the whole
/// sync over one row (CLAUDE.md #7/#8: fail closed on the *row*, never panic).
/// Serialized as a plain string so the storage column stays a simple TEXT.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum KnowledgeNodeKind {
    Discovery,
    Decision,
    Fact,
    Constraint,
    Artifact,
    Other(String),
}

impl KnowledgeNodeKind {
    /// Stable lowercase tag, identical to the serde wire/storage form.
    pub fn as_str(&self) -> &str {
        match self {
            KnowledgeNodeKind::Discovery => "discovery",
            KnowledgeNodeKind::Decision => "decision",
            KnowledgeNodeKind::Fact => "fact",
            KnowledgeNodeKind::Constraint => "constraint",
            KnowledgeNodeKind::Artifact => "artifact",
            KnowledgeNodeKind::Other(s) => s.as_str(),
        }
    }
}

impl FromStr for KnowledgeNodeKind {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "discovery" => KnowledgeNodeKind::Discovery,
            "decision" => KnowledgeNodeKind::Decision,
            "fact" => KnowledgeNodeKind::Fact,
            "constraint" => KnowledgeNodeKind::Constraint,
            "artifact" => KnowledgeNodeKind::Artifact,
            other => KnowledgeNodeKind::Other(other.to_string()),
        })
    }
}

impl Serialize for KnowledgeNodeKind {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for KnowledgeNodeKind {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        // Infallible: an unrecognized kind becomes `Other(s)` rather than an
        // error, so one forward-version row never fails the batch.
        Ok(s.parse().unwrap_or(KnowledgeNodeKind::Other(s)))
    }
}

/// Direction of a preference signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalDirection {
    Positive,
    Negative,
    Neutral,
}

/// Scope at which a consolidated preference applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreferenceScope {
    Workspace,
    Account,
    Global,
}

/// Lifecycle state of a reasoning session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    Completed,
    Abandoned,
}

/// Which engine produced a node/preference. Cloud is authoritative; the field
/// exists so cloud-derived rows are never re-uploaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeOrigin {
    Local,
    Cloud,
}

/// Where the conversation is in its arc. Drives Orkia's response posture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConversationPhase {
    EarlyExploration,
    ActiveDiscussion,
    NearingDecision,
    PostDecision,
}

impl ConversationPhase {
    pub fn label(self) -> &'static str {
        match self {
            ConversationPhase::EarlyExploration => "early-exploration",
            ConversationPhase::ActiveDiscussion => "active-discussion",
            ConversationPhase::NearingDecision => "nearing-decision",
            ConversationPhase::PostDecision => "post-decision",
        }
    }
}

/// A preference dimension. The cold pass emits a known taxonomy, but the set is
/// open — an unrecognized dimension round-trips losslessly through
/// [`Dimension::Other`] instead of becoming an opaque string. Serialized as a
/// plain string (so the storage column stays a simple TEXT).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Dimension {
    Verbosity,
    Language,
    Tone,
    Format,
    Risk,
    Tooling,
    Other(String),
}

impl Dimension {
    pub fn as_str(&self) -> &str {
        match self {
            Dimension::Verbosity => "verbosity",
            Dimension::Language => "language",
            Dimension::Tone => "tone",
            Dimension::Format => "format",
            Dimension::Risk => "risk",
            Dimension::Tooling => "tooling",
            Dimension::Other(s) => s.as_str(),
        }
    }
}

impl fmt::Display for Dimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Dimension {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "verbosity" => Dimension::Verbosity,
            "language" => Dimension::Language,
            "tone" => Dimension::Tone,
            "format" => Dimension::Format,
            "risk" => Dimension::Risk,
            "tooling" => Dimension::Tooling,
            other => Dimension::Other(other.to_string()),
        })
    }
}

impl From<String> for Dimension {
    fn from(s: String) -> Self {
        // Avoid an allocation for the known-variant case where possible.
        s.parse().unwrap_or(Dimension::Other(s))
    }
}

impl From<Dimension> for String {
    fn from(d: Dimension) -> Self {
        match d {
            Dimension::Other(s) => s,
            known => known.as_str().to_string(),
        }
    }
}

impl Serialize for Dimension {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Dimension {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Ok(Dimension::from(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip every closed graph enum: serialize → deserialize → equal.
    /// This is the wire/storage contract.
    #[test]
    fn closed_enums_round_trip() {
        macro_rules! rt {
            ($v:expr) => {{
                let json = serde_json::to_string(&$v).unwrap();
                let back = serde_json::from_str(&json).unwrap();
                assert_eq!($v, back, "round-trip failed for {}", json);
            }};
        }
        rt!(TurnRole::Tool);
        rt!(TurnRelation::ToolResult);
        rt!(TurnKind::UserPrompt);
        rt!(TurnKind::ToolCall("Read".into()));
        rt!(KnowledgeNodeKind::Decision);
        rt!(SignalDirection::Positive);
        rt!(PreferenceScope::Account);
        rt!(SessionStatus::Completed);
        rt!(NodeOrigin::Cloud);
        rt!(ConversationPhase::NearingDecision);
    }

    #[test]
    fn link_enum_wire_form_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&TurnRelation::FollowUp).unwrap(),
            "\"follow_up\""
        );
    }

    #[test]
    fn turn_kind_tool_call_carries_tool_as_field() {
        let json = serde_json::to_string(&TurnKind::ToolCall("Bash".into())).unwrap();
        assert_eq!(json, r#"{"kind":"tool_call","tool":"Bash"}"#);
        assert_eq!(TurnKind::ToolCall("Bash".into()).tool(), Some("Bash"));
        assert_eq!(
            TurnKind::ToolCall("Bash".into()).discriminant(),
            "tool_call"
        );
    }

    #[test]
    fn unknown_closed_variant_fails_closed() {
        // A bogus role must NOT silently become a default — it errors so the
        // caller can skip the record.
        let r: Result<TurnRole, _> = serde_json::from_str("\"wizard\"");
        assert!(r.is_err());
    }

    #[test]
    fn node_kind_known_unknown_and_cross_vendor_round_trip() {
        // Both vendors' known variants serialize to their lowercase tag.
        assert_eq!(
            serde_json::to_string(&KnowledgeNodeKind::Fact).unwrap(),
            "\"fact\""
        );
        assert_eq!(
            serde_json::to_string(&KnowledgeNodeKind::Artifact).unwrap(),
            "\"artifact\""
        );

        // A cloud-minted `artifact` deserializes into the union variant, not Other.
        let from_cloud: KnowledgeNodeKind = serde_json::from_str("\"artifact\"").unwrap();
        assert_eq!(from_cloud, KnowledgeNodeKind::Artifact);

        // An unknown/forward-version kind round-trips losslessly via Other — it
        // must NOT error (one row never fails the whole sync).
        let exotic: KnowledgeNodeKind = serde_json::from_str("\"hypothesis\"").unwrap();
        assert_eq!(exotic, KnowledgeNodeKind::Other("hypothesis".into()));
        assert_eq!(serde_json::to_string(&exotic).unwrap(), "\"hypothesis\"");
    }

    #[test]
    fn dimension_known_and_unknown_round_trip() {
        let known = Dimension::Verbosity;
        assert_eq!(serde_json::to_string(&known).unwrap(), "\"verbosity\"");
        let back: Dimension = serde_json::from_str("\"verbosity\"").unwrap();
        assert_eq!(back, Dimension::Verbosity);

        // Unknown dimension round-trips losslessly via Other.
        let exotic: Dimension = serde_json::from_str("\"emoji_density\"").unwrap();
        assert_eq!(exotic, Dimension::Other("emoji_density".into()));
        assert_eq!(serde_json::to_string(&exotic).unwrap(), "\"emoji_density\"");
    }
}

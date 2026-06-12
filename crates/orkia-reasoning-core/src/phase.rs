// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! the Markdown block appended to an agent's system prompt. No I/O — the
//! daemon builds the context and hands it in.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::enums::{ConversationPhase, Dimension, KnowledgeNodeKind, TurnRole};

/// A turn after classification, used to infer the conversation phase.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClassifiedTurn {
    pub seq: i32,
    pub role: TurnRole,
    pub author_name: String,
    /// Classifier tag (`"inflexion"`, `"cristallisation"`, …). Stays a free
    /// string: it is an open, evolving classifier taxonomy, not a graph edge.
    pub turn_type: Option<String>,
    pub summary: Option<String>,
    #[serde(default)]
    pub turn_id: Option<Uuid>,
}

/// A consolidated knowledge node, summarized for prompt rendering.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KnowledgeNodeSummary {
    pub kind: KnowledgeNodeKind,
    pub summary: String,
    pub confidence: f64,
}

/// An effective user preference, summarized for prompt rendering.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserPreferenceSummary {
    pub dimension: Dimension,
    pub value: String,
    pub confidence: f64,
}

/// Structural view of a conversation built from the reasoning graph. Optional
/// at call sites — callers fall back to raw history when this is absent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReasoningContext {
    pub classified_turns: Vec<ClassifiedTurn>,
    pub relevant_knowledge: Vec<KnowledgeNodeSummary>,
    pub user_preferences: Vec<UserPreferenceSummary>,
    pub inflexion_points: Vec<String>,
    pub conversation_phase: ConversationPhase,
}

/// - Fewer than 5 turns → `EarlyExploration` (signals are sparse).
/// - Any `cristallisation` turn → `PostDecision`.
/// - Any `inflexion` turn → `NearingDecision`.
/// - Otherwise → `ActiveDiscussion`.
pub fn infer_phase(turns: &[ClassifiedTurn]) -> ConversationPhase {
    if turns.len() < 5 {
        return ConversationPhase::EarlyExploration;
    }
    let mut has_cristallisation = false;
    let mut has_inflexion = false;
    for t in turns {
        match t.turn_type.as_deref() {
            Some("cristallisation") => has_cristallisation = true,
            Some("inflexion") => has_inflexion = true,
            _ => {}
        }
    }
    if has_cristallisation {
        ConversationPhase::PostDecision
    } else if has_inflexion {
        ConversationPhase::NearingDecision
    } else {
        ConversationPhase::ActiveDiscussion
    }
}

/// Render the reasoning context as a Markdown block for an agent's system
/// prompt. Empty sections are omitted so the prompt stays signal-dense.
pub fn render_reasoning_block(ctx: &ReasoningContext) -> String {
    let mut out = String::from("\n--- REASONING CONTEXT ---\n");
    out.push_str(&format!(
        "Conversation phase: {}\n",
        ctx.conversation_phase.label()
    ));
    out.push_str(&format!(
        "Inflexion count: {}\n",
        ctx.inflexion_points.len()
    ));

    if !ctx.inflexion_points.is_empty() {
        out.push_str("\nKey inflexion points:\n");
        for s in &ctx.inflexion_points {
            out.push_str(&format!("- {s}\n"));
        }
    }

    if !ctx.relevant_knowledge.is_empty() {
        out.push_str("\nKnown decisions & constraints (prior sessions):\n");
        for kn in &ctx.relevant_knowledge {
            out.push_str(&format!(
                "- [{}] {} (confidence: {:.0}%)\n",
                kn.kind.as_str(),
                kn.summary,
                kn.confidence * 100.0
            ));
        }
    }

    if !ctx.user_preferences.is_empty() {
        out.push_str("\nUser preferences:\n");
        for p in &ctx.user_preferences {
            out.push_str(&format!(
                "- {}: {} (confidence: {:.0}%)\n",
                p.dimension,
                p.value,
                p.confidence * 100.0
            ));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(seq: i32, ty: Option<&str>) -> ClassifiedTurn {
        ClassifiedTurn {
            seq,
            role: TurnRole::User,
            author_name: "Killix".into(),
            turn_type: ty.map(String::from),
            summary: None,
            turn_id: None,
        }
    }

    #[test]
    fn fewer_than_five_is_early_exploration() {
        let turns = vec![t(1, Some("exploration")), t(2, Some("inflexion"))];
        assert_eq!(infer_phase(&turns), ConversationPhase::EarlyExploration);
    }

    #[test]
    fn cristallisation_wins_over_inflexion() {
        let turns = (1..=6)
            .map(|i| {
                t(
                    i,
                    if i == 3 {
                        Some("inflexion")
                    } else if i == 5 {
                        Some("cristallisation")
                    } else {
                        None
                    },
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(infer_phase(&turns), ConversationPhase::PostDecision);
    }

    #[test]
    fn inflexion_only_yields_nearing_decision() {
        let turns = (1..=6)
            .map(|i| t(i, if i == 4 { Some("inflexion") } else { None }))
            .collect::<Vec<_>>();
        assert_eq!(infer_phase(&turns), ConversationPhase::NearingDecision);
    }

    #[test]
    fn no_strong_signals_is_active_discussion() {
        let turns = (1..=6).map(|i| t(i, Some("routine"))).collect::<Vec<_>>();
        assert_eq!(infer_phase(&turns), ConversationPhase::ActiveDiscussion);
    }

    #[test]
    fn render_block_omits_empty_sections() {
        let rc = ReasoningContext {
            classified_turns: vec![],
            relevant_knowledge: vec![],
            user_preferences: vec![],
            inflexion_points: vec![],
            conversation_phase: ConversationPhase::EarlyExploration,
        };
        let s = render_reasoning_block(&rc);
        assert!(s.contains("early-exploration"));
        assert!(!s.contains("Key inflexion points"));
        assert!(!s.contains("Known decisions"));
        assert!(!s.contains("User preferences"));
    }

    #[test]
    fn render_block_includes_populated_sections() {
        let rc = ReasoningContext {
            classified_turns: vec![],
            relevant_knowledge: vec![KnowledgeNodeSummary {
                kind: KnowledgeNodeKind::Decision,
                summary: "use sqlite".into(),
                confidence: 0.8,
            }],
            user_preferences: vec![UserPreferenceSummary {
                dimension: Dimension::Verbosity,
                value: "concise".into(),
                confidence: 0.9,
            }],
            inflexion_points: vec!["switched store".into()],
            conversation_phase: ConversationPhase::NearingDecision,
        };
        let s = render_reasoning_block(&rc);
        assert!(s.contains("[decision] use sqlite (confidence: 80%)"));
        assert!(s.contains("verbosity: concise (confidence: 90%)"));
        assert!(s.contains("- switched store"));
    }
}

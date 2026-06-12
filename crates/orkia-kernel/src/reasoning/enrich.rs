// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! Enrich at spawn. Composes the `<user_preferences>` block (from the
//! lock-free [`PreferenceCache`], warmed from the local store) and the
//! `--- REASONING CONTEXT ---` block (phase + recent nodes) onto an agent's
//! system prompt. Pure and synchronous — **never** a network call on the
//! spawn path.

use uuid::Uuid;

use orkia_reasoning_core::{
    PreferenceCache, ReasoningContext, inject_preferences, render_reasoning_block,
};

/// Append preference + reasoning-context blocks to `base`, scoped to a
/// workspace. `context` is the per-scope structural view (already built by the
/// consumer); when absent, only the preference block is added.
pub fn enrich_system_prompt(
    base: &str,
    cache: &PreferenceCache,
    workspace_id: Uuid,
    context: Option<&ReasoningContext>,
) -> String {
    let prefs = cache.get(workspace_id).unwrap_or_default();
    let mut out = inject_preferences(base, &prefs);
    if let Some(ctx) = context {
        out.push_str(&render_reasoning_block(ctx));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_reasoning_core::dto::PreferenceDto;
    use orkia_reasoning_core::enums::{
        ConversationPhase, Dimension, KnowledgeNodeKind, PreferenceScope,
    };
    use orkia_reasoning_core::phase::{KnowledgeNodeSummary, UserPreferenceSummary};

    fn cache_with(ws: Uuid, prefs: Vec<PreferenceDto>) -> PreferenceCache {
        let c = PreferenceCache::new();
        c.put(ws, prefs);
        c
    }

    fn pref(conf: f32) -> PreferenceDto {
        PreferenceDto {
            dimension: Dimension::Verbosity,
            value: "concise".into(),
            confidence: conf,
            observation_count: 3,
            scope: PreferenceScope::Workspace,
        }
    }

    #[test]
    fn injects_high_confidence_pref() {
        let ws = Uuid::from_u128(1);
        let cache = cache_with(ws, vec![pref(0.9)]);
        let out = enrich_system_prompt("You are Orkia.", &cache, ws, None);
        assert!(out.contains("<user_preferences>"));
        assert!(out.contains("verbosity: concise"));
    }

    #[test]
    fn no_prefs_and_no_context_is_passthrough() {
        let ws = Uuid::from_u128(1);
        let cache = PreferenceCache::new();
        let out = enrich_system_prompt("base", &cache, ws, None);
        assert_eq!(out, "base");
    }

    #[test]
    fn appends_reasoning_context_block() {
        let ws = Uuid::from_u128(1);
        let cache = cache_with(ws, vec![pref(0.9)]);
        let ctx = ReasoningContext {
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
            inflexion_points: vec![],
            conversation_phase: ConversationPhase::NearingDecision,
        };
        let out = enrich_system_prompt("base", &cache, ws, Some(&ctx));
        assert!(out.contains("<user_preferences>"));
        assert!(out.contains("--- REASONING CONTEXT ---"));
        assert!(out.contains("nearing-decision"));
        assert!(out.contains("[decision] use sqlite"));
    }
}

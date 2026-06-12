// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! Pure preference injection. Fetching/caching lives in the client crate
//! (it does I/O); this module only transforms a system prompt given the
//! preferences already in hand.

use std::fmt::Write;

use crate::dto::PreferenceDto;

const MIN_CONFIDENCE: f32 = 0.6;

/// Append a `<user_preferences>` block to `system_prompt`, listing only
/// preferences with confidence ≥ 0.6. Returns the prompt unchanged when no
/// preference clears the threshold.
pub fn inject_preferences(system_prompt: &str, prefs: &[PreferenceDto]) -> String {
    let high: Vec<&PreferenceDto> = prefs
        .iter()
        .filter(|p| p.confidence >= MIN_CONFIDENCE)
        .collect();
    if high.is_empty() {
        return system_prompt.to_string();
    }
    let mut out = String::with_capacity(system_prompt.len() + 256);
    out.push_str(system_prompt);
    if !system_prompt.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("\n<user_preferences>\n");
    for p in high {
        let _ = writeln!(
            out,
            "- {dim}: {val} (confidence: {conf:.2})",
            dim = p.dimension,
            val = p.value,
            conf = p.confidence
        );
    }
    out.push_str("</user_preferences>\n");
    out
}

/// instruction that teaches the agent to pull from the KG before acting. It
/// references the `recall` MCP tool, which is registered **only** for premium,
/// so this block must be appended only on the gate-open path — the caller
/// (`enrich_active`) gates it; this function is the pure renderer.
pub const KNOWLEDGE_PROTOCOL: &str = "\n## Knowledge Protocol\n\
You have access to a project knowledge graph via the `recall` tool.\n\
BEFORE starting work on any task, call recall with the relevant topic.\n\
BEFORE proposing any architecture or technology decision, call recall to check\n\
for existing decisions and eliminated alternatives in this project.\n\
NEVER propose a solution without first checking if it was previously considered and rejected.\n\
If recall returns a rejected alternative, do NOT re-propose it. State what was decided instead.\n";

/// Append the [`KNOWLEDGE_PROTOCOL`] block to `system_prompt`, normalizing the
/// boundary newline. Pure; the gate decision lives at the call site.
pub fn append_knowledge_protocol(system_prompt: &str) -> String {
    let mut out = String::with_capacity(system_prompt.len() + KNOWLEDGE_PROTOCOL.len() + 1);
    out.push_str(system_prompt);
    if !system_prompt.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(KNOWLEDGE_PROTOCOL);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enums::{Dimension, PreferenceScope};

    fn pref(dim: Dimension, val: &str, confidence: f32) -> PreferenceDto {
        PreferenceDto {
            dimension: dim,
            value: val.into(),
            confidence,
            observation_count: 1,
            scope: PreferenceScope::Workspace,
        }
    }

    #[test]
    fn filters_low_confidence() {
        let prompt = "You are a helpful assistant.";
        let prefs = vec![
            pref(Dimension::Verbosity, "concise", 0.9),
            pref(Dimension::Language, "fr", 0.3),
        ];
        let out = inject_preferences(prompt, &prefs);
        assert!(out.contains("verbosity"));
        assert!(!out.contains("language"));
    }

    #[test]
    fn empty_when_no_high_conf() {
        let out = inject_preferences("system", &[pref(Dimension::Tone, "warm", 0.1)]);
        assert_eq!(out, "system");
    }

    #[test]
    fn no_prefs_returns_original() {
        assert_eq!(inject_preferences("system", &[]), "system");
    }

    #[test]
    fn knowledge_protocol_appends_once_and_normalizes_boundary() {
        let out = append_knowledge_protocol("base");
        assert!(out.contains("## Knowledge Protocol"));
        assert!(out.contains("`recall` tool"));
        // Boundary newline inserted exactly once (no blank-line doubling).
        assert!(out.starts_with("base\n\n## Knowledge Protocol"));
        // Already-terminated input is not double-spaced beyond the block's own gap.
        let out2 = append_knowledge_protocol("base\n");
        assert!(out2.starts_with("base\n\n## Knowledge Protocol"));
    }
}

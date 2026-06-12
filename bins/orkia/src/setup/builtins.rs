// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Hardcoded archetype list used when no registry cache is available
//! (`--offline` cold start, or remote unreachable on first run).
//!
//! The system prompts themselves live in
//! `crates/orkia-builtin/src/agent_templates.rs` — this module only
//! provides the wizard-facing metadata (description, suggested names,
//! preferred CLI). When the user picks one of these, `scaffold` calls
//! `agent_templates::generate_prompt_template` rather than reading from
//! disk.

use super::registry::{ArchetypeMeta, ArchetypeSource};

const BUILTINS: &[(&str, &str, &[&str], &[&str])] = &[
    (
        "software-eng",
        "Software engineer — writes, reviews, refactors code",
        &["faye", "rex", "nova", "kai"],
        &["claude", "codex"],
    ),
    (
        "qa-testing",
        "QA engineer — tests, coverage, code review",
        &["sage", "iris", "mira"],
        &["claude", "codex"],
    ),
    (
        "devops",
        "DevOps engineer — CI/CD, infra, deploys, monitoring",
        &["killua", "atlas", "ash"],
        &["codex", "claude"],
    ),
    (
        "business-ops",
        "Business operations — reports, analysis, decks",
        &["juno", "lyra", "wren"],
        &["claude", "gemini"],
    ),
    (
        "data-analysis",
        "Data analyst — metrics, dashboards, SQL",
        &["ada", "nova", "tess"],
        &["claude", "gemini"],
    ),
];

pub fn builtin_archetypes() -> Vec<ArchetypeMeta> {
    BUILTINS
        .iter()
        .map(|(name, desc, names, preferred)| ArchetypeMeta {
            name: (*name).into(),
            description: (*desc).into(),
            suggested_names: names.iter().map(|s| (*s).to_string()).collect(),
            preferred_cli: preferred.iter().map(|s| (*s).to_string()).collect(),
            is_community: false,
            source: ArchetypeSource::Builtin,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_builtin::agent_templates::KNOWN_ARCHETYPES;

    #[test]
    fn builtins_match_known_archetype_templates() {
        let builtins = builtin_archetypes();
        for b in &builtins {
            assert!(
                KNOWN_ARCHETYPES.contains(&b.name.as_str()),
                "builtin '{}' has no matching prompt template",
                b.name,
            );
        }
        assert_eq!(builtins.len(), 5);
    }
}

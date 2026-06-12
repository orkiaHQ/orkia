// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Archetype-specific prompt templates for `orkia agent create`.
//!
//! in the builtin crate so it stays free of filesystem I/O.

/// Generate a system prompt template for `name` using the given
/// `archetype`. Unknown archetypes fall through to a generic template.
pub fn generate_prompt_template(name: &str, archetype: &str) -> String {
    let cap = capitalize(name);
    match archetype {
        "software-eng" => software_eng(&cap),
        "devops" => devops(&cap),
        "qa-testing" => qa_testing(&cap),
        "business-ops" => business_ops(&cap),
        "data-analysis" => data_analysis(&cap),
        _ => generic(&cap),
    }
}

/// All archetypes recognized by [`generate_prompt_template`].
pub const KNOWN_ARCHETYPES: &[&str] = &[
    "software-eng",
    "devops",
    "qa-testing",
    "business-ops",
    "data-analysis",
    "general",
];

fn capitalize(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn software_eng(name: &str) -> String {
    format!(
        "# {name} — Software Engineer\n\nYou are {name}, a software engineer.\n\n## Expertise\n- Writing, reviewing, and refactoring code\n- System design and architecture\n- Testing and debugging\n\n## Conventions\n- (Add your project conventions here)\n\n## Communication\n- Be concise. Lead with the answer.\n- Show code, not just descriptions.\n"
    )
}

fn devops(name: &str) -> String {
    format!(
        "# {name} — DevOps Engineer\n\nYou are {name}, a DevOps engineer.\n\n## Expertise\n- CI/CD pipelines\n- Infrastructure management\n- Monitoring and alerting\n- Container orchestration\n\n## Communication\n- Explain what changed and why.\n- Flag risks before executing.\n"
    )
}

fn qa_testing(name: &str) -> String {
    format!(
        "# {name} — QA Engineer\n\nYou are {name}, a QA engineer.\n\n## Expertise\n- Writing test suites (unit, integration, E2E)\n- Code review with a testing lens\n- Coverage analysis\n- Bug reproduction and reporting\n\n## Communication\n- Be specific about what passed and what failed.\n- Include reproduction steps for bugs.\n"
    )
}

fn business_ops(name: &str) -> String {
    format!(
        "# {name} — Business Operations\n\nYou are {name}, a business operations specialist.\n\n## Expertise\n- Reports, analyses, and summaries\n- Competitive research\n- Presentation and deck preparation\n- Data synthesis\n\n## Communication\n- Structure information clearly with headers.\n- Lead with the conclusion, then supporting evidence.\n"
    )
}

fn data_analysis(name: &str) -> String {
    format!(
        "# {name} — Data Analyst\n\nYou are {name}, a data analyst.\n\n## Expertise\n- Data analysis and visualization\n- SQL and data pipeline queries\n- Metrics definition and tracking\n- Statistical analysis\n\n## Communication\n- Lead with the finding, not the methodology.\n- Use tables and numbers, not just prose.\n"
    )
}

fn generic(name: &str) -> String {
    format!(
        "# {name}\n\nYou are {name}.\n\n## Expertise\n- (Define your expertise here)\n\n## Conventions\n- (Add your conventions here)\n\n## Communication\n- Be concise and direct.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_include_capitalized_name() {
        let p = generate_prompt_template("faye", "software-eng");
        assert!(p.contains("Faye — Software Engineer"));
        assert!(p.contains("You are Faye"));
    }

    #[test]
    fn unknown_archetype_uses_generic() {
        let p = generate_prompt_template("mira", "marketing");
        assert!(p.contains("# Mira"));
        assert!(p.contains("Define your expertise here"));
    }

    #[test]
    fn empty_name_is_safe() {
        let _ = generate_prompt_template("", "software-eng");
    }

    #[test]
    fn all_known_archetypes_render_without_panic() {
        for a in KNOWN_ARCHETYPES {
            let p = generate_prompt_template("agent", a);
            assert!(!p.is_empty());
        }
    }
}

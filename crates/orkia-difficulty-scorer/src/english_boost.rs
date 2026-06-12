// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

#[derive(Debug, Clone, Default)]
pub struct EnglishBoostSignals {
    pub constraint_count: u32,
    pub multi_step_count: u32,
    pub negation_count: u32,
    pub conditional_count: u32,
    pub comparison_count: u32,
    pub explanation_depth: u32,
}

pub fn extract_english_boost(prompt: &str) -> EnglishBoostSignals {
    let lower = prompt.to_lowercase();

    EnglishBoostSignals {
        constraint_count: count_matches(
            &lower,
            &[
                "must",
                "should",
                "shall",
                "exactly",
                "at least",
                "at most",
                "no more than",
                "no less than",
                "required",
                "mandatory",
                "ensure",
                "guarantee",
                "strictly",
            ],
        ),
        multi_step_count: count_matches(
            &lower,
            &[
                "first",
                "then",
                "next",
                "finally",
                "step 1",
                "step 2",
                "step 3",
                "after that",
                "followed by",
                "before you",
                "start by",
                "begin with",
            ],
        ),
        negation_count: count_matches(
            &lower,
            &[
                "don't",
                "do not",
                "never",
                "without",
                "except",
                "unless",
                "must not",
                "avoid",
                "shouldn't",
                "should not",
                "cannot",
                "can't",
                "won't",
                "will not",
                "neither",
                "nor",
            ],
        ),
        conditional_count: count_matches(
            &lower,
            &[
                "if ",
                "else ",
                "otherwise",
                "unless",
                "in case",
                "when ",
                "assuming",
                "given that",
                "provided that",
                "suppose",
                "in the event",
            ],
        ),
        comparison_count: count_matches(
            &lower,
            &[
                "compare",
                " vs ",
                "versus",
                "difference between",
                "pros and cons",
                "tradeoff",
                "trade-off",
                "better than",
                "worse than",
                "advantages",
                "disadvantages",
            ],
        ),
        explanation_depth: count_matches(
            &lower,
            &[
                "why ",
                "how does",
                "how do",
                "explain",
                "what causes",
                "reason for",
                "mechanism behind",
                "in depth",
                "in detail",
                "elaborate",
                "break down",
                "walk me through",
            ],
        ),
    }
}

fn count_matches(text: &str, patterns: &[&str]) -> u32 {
    patterns
        .iter()
        .map(|p| text.matches(p).count() as u32)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constraints_detected() {
        let s = extract_english_boost("You must ensure the function is strictly correct");
        assert!(s.constraint_count >= 3);
    }

    #[test]
    fn multi_step_detected() {
        let s = extract_english_boost("First validate input, then process, finally return");
        assert!(s.multi_step_count >= 3);
    }

    #[test]
    fn negations_detected() {
        let s = extract_english_boost(
            "Don't use global variables. Never mutate state without locking.",
        );
        assert!(s.negation_count >= 2);
    }

    #[test]
    fn empty_prompt_zero() {
        let s = extract_english_boost("");
        assert_eq!(s.constraint_count, 0);
        assert_eq!(s.multi_step_count, 0);
        assert_eq!(s.negation_count, 0);
    }
}

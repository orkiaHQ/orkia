// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

use crate::{DifficultySignals, ModelTier};

/// Difficulty threshold above which the Frontier tier is required.
const FRONTIER_THRESHOLD: f32 = 0.70;
/// Difficulty threshold above which the Performance tier is required.
const PERFORMANCE_THRESHOLD: f32 = 0.35;

#[derive(Clone)]
pub struct DifficultyWeights {
    pub w_length: f32,
    pub w_code: f32,
    pub w_structure: f32,
    pub w_vocabulary: f32,
    pub w_formatting: f32,
    pub w_math: f32,
    pub w_context: f32,
    pub w_english_boost: f32,
}

impl Default for DifficultyWeights {
    fn default() -> Self {
        Self {
            w_length: 0.15,
            w_code: 0.20,
            w_structure: 0.15,
            w_vocabulary: 0.10,
            w_formatting: 0.10,
            w_math: 0.10,
            w_context: 0.05,
            w_english_boost: 0.15,
        }
    }
}

pub struct DifficultyScorer {
    weights: DifficultyWeights,
}

impl Default for DifficultyScorer {
    fn default() -> Self {
        Self::new()
    }
}

impl DifficultyScorer {
    pub fn new() -> Self {
        Self {
            weights: DifficultyWeights::default(),
        }
    }

    pub fn with_weights(weights: DifficultyWeights) -> Self {
        Self { weights }
    }

    pub fn score(&self, signals: &DifficultySignals) -> f32 {
        let w = &self.weights;
        let s = &signals.structural;

        let length = (s.token_count as f32 / 200.0).min(1.0);

        let code = if s.code_block_count > 0 || s.code_line_count > 0 {
            let block_score = (s.code_block_count as f32 / 3.0).min(1.0);
            let line_score = (s.code_line_count as f32 / 50.0).min(1.0);
            let inline_score = (s.inline_code_count as f32 / 5.0).min(1.0);
            block_score * 0.3 + line_score * 0.5 + inline_score * 0.2
        } else {
            0.0
        };

        let structure = {
            let nest = (s.nesting_depth as f32 / 4.0).min(1.0);
            let sections = (s.section_count as f32 / 3.0).min(1.0);
            let lists = (s.list_marker_count as f32 / 5.0).min(1.0);
            let table = if s.has_table { 0.5 } else { 0.0 };
            let json = if s.has_json { 0.3 } else { 0.0 };
            nest * 0.3 + sections * 0.2 + lists * 0.2 + table * 0.15 + json * 0.15
        };

        let vocabulary = s.unique_token_ratio;

        let formatting = {
            let punct = (s.punctuation_density * 5.0).min(1.0);
            let newlines = (s.newline_count as f32 / 20.0).min(1.0);
            let indent = (s.indentation_levels as f32 / 4.0).min(1.0);
            punct * 0.3 + newlines * 0.4 + indent * 0.3
        };

        let math = {
            let symbols = (s.math_symbol_count as f32 / 3.0).min(1.0);
            let parens = (s.parenthetical_depth as f32 / 3.0).min(1.0);
            symbols * 0.6 + parens * 0.4
        };

        let context = {
            let urls = (s.url_count as f32 / 2.0).min(1.0);
            let quotes = (s.quoted_block_count as f32 / 2.0).min(1.0);
            urls * 0.5 + quotes * 0.5
        };

        let english_boost = if signals.is_english {
            let eb = &signals.english_boost;
            let constraints = (eb.constraint_count as f32 / 3.0).min(1.0);
            let steps = (eb.multi_step_count as f32 / 2.0).min(1.0);
            let negations = (eb.negation_count as f32 / 3.0).min(1.0);
            let conditionals = (eb.conditional_count as f32 / 2.0).min(1.0);
            let comparisons = (eb.comparison_count as f32 / 2.0).min(1.0);
            let depth = (eb.explanation_depth as f32 / 2.0).min(1.0);
            constraints * 0.25
                + steps * 0.20
                + negations * 0.10
                + conditionals * 0.15
                + comparisons * 0.15
                + depth * 0.15
        } else {
            0.0
        };

        let english_weight = if signals.is_english {
            w.w_english_boost
        } else {
            0.0
        };
        let total_weight = w.w_length
            + w.w_code
            + w.w_structure
            + w.w_vocabulary
            + w.w_formatting
            + w.w_math
            + w.w_context
            + english_weight;

        let weighted = length * w.w_length
            + code * w.w_code
            + structure * w.w_structure
            + vocabulary * w.w_vocabulary
            + formatting * w.w_formatting
            + math * w.w_math
            + context * w.w_context
            + english_boost * english_weight;

        // Guard against all-zero weights: `0.0 / 0.0 = NaN`, which then slips
        // through `clamp` and silently scores everything as the lowest tier
        // (BUG-085).
        let raw = if total_weight > 0.0 {
            weighted / total_weight
        } else {
            0.0
        };

        raw.clamp(0.0, 1.0)
    }
}

/// Map a difficulty score to the minimum required [`ModelTier`].
pub fn min_tier(difficulty: f32) -> ModelTier {
    if difficulty >= FRONTIER_THRESHOLD {
        ModelTier::Frontier
    } else if difficulty >= PERFORMANCE_THRESHOLD {
        ModelTier::Performance
    } else {
        ModelTier::Economy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scorer() -> DifficultyScorer {
        DifficultyScorer::new()
    }

    #[test]
    fn trivial_prompt_low_difficulty() {
        let signals = DifficultySignals::extract("What is 2+2?");
        let score = scorer().score(&signals);
        assert!(
            score < 0.2,
            "Simple question should be trivial, got {score}"
        );
    }

    #[test]
    fn code_prompt_medium_difficulty() {
        let prompt = "Write a Python function to sort a list using quicksort";
        let signals = DifficultySignals::extract(prompt);
        let score = scorer().score(&signals);
        assert!(
            score > 0.1 && score < 0.6,
            "Standard code task should be medium, got {score}"
        );
    }

    #[test]
    fn complex_code_prompt_high_difficulty() {
        let prompt = "Implement a distributed lock manager with:\n\
            - Lease renewal with configurable TTL\n\
            - Fencing tokens to prevent stale locks\n\
            - Must handle network partitions gracefully\n\
            - If a node crashes, locks must be released within 30 seconds\n\
            - Include unit tests for edge cases\n\
            ```rust\n// existing code to refactor\nstruct Lock { id: u64, ttl: Duration }\nimpl Lock {\n    fn acquire(&mut self) -> Result<()> { todo!() }\n    fn release(&mut self) -> Result<()> { todo!() }\n}\n```";
        let signals = DifficultySignals::extract(prompt);
        let score = scorer().score(&signals);
        assert!(
            score > 0.35,
            "Complex constrained code task should be hard, got {score}"
        );
    }

    #[test]
    fn math_prompt_high_difficulty() {
        let prompt = "Prove that ∑(1/n²) = π²/6 using the Basel problem approach";
        let signals = DifficultySignals::extract(prompt);
        let score = scorer().score(&signals);
        assert!(
            score > 0.2,
            "Math proof should register difficulty, got {score}"
        );
    }

    #[test]
    fn multilingual_code_prompt() {
        let prompt = "写一个Python函数来排序列表\n```python\ndef sort_list(arr):\n    pass\n```";
        let signals = DifficultySignals::extract(prompt);
        assert_eq!(signals.structural.code_block_count, 1);
        assert_eq!(signals.structural.code_line_count, 2);
        assert!(!signals.is_english);
        let score = scorer().score(&signals);
        assert!(
            score > 0.15,
            "Code task in Chinese should still register difficulty, got {score}"
        );
    }

    #[test]
    fn english_boost_increases_score() {
        let base = "Write a function";
        let boosted = "Write a function that must handle all edge cases. \
                       First validate input, then process, finally return. \
                       If the input is invalid, throw an exception. \
                       Do not use global variables.";
        let s1 = DifficultySignals::extract(base);
        let s2 = DifficultySignals::extract(boosted);
        let score1 = scorer().score(&s1);
        let score2 = scorer().score(&s2);
        assert!(
            score2 > score1 + 0.04,
            "English constraints should boost difficulty: base={score1}, boosted={score2}"
        );
    }

    #[test]
    fn non_english_still_captures_structure() {
        let prompt = "Erstelle eine Funktion:\n\
                      1. Eingabe validieren\n\
                      2. Daten verarbeiten\n\
                      3. Ergebnis zurückgeben\n\
                      ```python\ndef process(data):\n    pass\n```";
        let signals = DifficultySignals::extract(prompt);
        assert!(!signals.is_english);
        assert!(signals.structural.list_marker_count >= 3);
        assert_eq!(signals.structural.code_block_count, 1);
        let score = scorer().score(&signals);
        assert!(
            score > 0.25,
            "German structured prompt should register medium difficulty, got {score}"
        );
    }

    #[test]
    fn min_tier_thresholds() {
        assert_eq!(min_tier(0.0), ModelTier::Economy);
        assert_eq!(min_tier(0.34), ModelTier::Economy);
        assert_eq!(min_tier(0.35), ModelTier::Performance);
        assert_eq!(min_tier(0.69), ModelTier::Performance);
        assert_eq!(min_tier(0.70), ModelTier::Frontier);
        assert_eq!(min_tier(1.0), ModelTier::Frontier);
    }

    #[test]
    fn score_always_in_range() {
        let long = "a ".repeat(1000);
        let prompts: &[&str] = &["", "hi", &long];
        let s = scorer();
        for p in prompts {
            let signals = DifficultySignals::extract(p);
            let score = s.score(&signals);
            assert!((0.0..=1.0).contains(&score), "Score out of range: {score}");
        }
    }
}

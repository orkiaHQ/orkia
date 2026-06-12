// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]

pub mod english_boost;
pub mod language_detect;
pub mod scorer;
pub mod structural;

pub use english_boost::EnglishBoostSignals;
pub use scorer::{DifficultyScorer, DifficultyWeights, min_tier};
pub use structural::StructuralSignals;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModelTier {
    Economy,
    Performance,
    Frontier,
}

#[derive(Debug, Clone)]
pub struct DifficultySignals {
    pub structural: StructuralSignals,
    pub english_boost: EnglishBoostSignals,
    pub is_english: bool,
}

impl DifficultySignals {
    pub fn extract(prompt: &str) -> Self {
        let structural = structural::extract_structural(prompt);
        let is_english = language_detect::detect_english(prompt);
        let english_boost = if is_english {
            english_boost::extract_english_boost(prompt)
        } else {
            EnglishBoostSignals::default()
        };

        Self {
            structural,
            english_boost,
            is_english,
        }
    }
}

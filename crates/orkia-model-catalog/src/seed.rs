// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

use crate::types::{IntentQuality, ModelProfile};

fn iq(intent: &str, quality_score: f32) -> IntentQuality {
    IntentQuality {
        intent: intent.to_owned(),
        quality_score,
    }
}

pub fn seed() -> Vec<ModelProfile> {
    vec![
        // ── Anthropic ───────────────────────────────────────────
        ModelProfile {
            model_id: "claude-opus-4-6".into(),
            provider: "anthropic".into(),
            tier: "frontier".into(),
            context_window: 200_000,
            input_cost_per_m: 15.0,
            output_cost_per_m: 75.0,
            supports_vision: true,
            supports_tools: true,
            intents: vec![
                iq("code_generation", 0.96),
                iq("reasoning", 0.97),
                iq("code_review", 0.94),
                iq("translation", 0.88),
                iq("summarization", 0.90),
                iq("classification", 0.91),
                iq("extraction", 0.92),
            ],
        },
        ModelProfile {
            model_id: "claude-sonnet-4-6".into(),
            provider: "anthropic".into(),
            tier: "performance".into(),
            context_window: 200_000,
            input_cost_per_m: 3.0,
            output_cost_per_m: 15.0,
            supports_vision: true,
            supports_tools: true,
            intents: vec![
                iq("code_generation", 0.91),
                iq("reasoning", 0.89),
                iq("code_review", 0.90),
                iq("translation", 0.85),
                iq("summarization", 0.87),
                iq("classification", 0.88),
                iq("extraction", 0.89),
            ],
        },
        ModelProfile {
            model_id: "claude-haiku-4-5".into(),
            provider: "anthropic".into(),
            tier: "economy".into(),
            context_window: 200_000,
            input_cost_per_m: 0.25,
            output_cost_per_m: 1.25,
            supports_vision: true,
            supports_tools: true,
            intents: vec![
                iq("code_generation", 0.72),
                iq("reasoning", 0.68),
                iq("code_review", 0.70),
                iq("translation", 0.80),
                iq("summarization", 0.82),
                iq("classification", 0.84),
                iq("extraction", 0.83),
            ],
        },
        // ── OpenAI ──────────────────────────────────────────────
        ModelProfile {
            model_id: "gpt-4o".into(),
            provider: "openai".into(),
            tier: "frontier".into(),
            context_window: 128_000,
            input_cost_per_m: 2.5,
            output_cost_per_m: 10.0,
            supports_vision: true,
            supports_tools: true,
            intents: vec![
                iq("code_generation", 0.92),
                iq("reasoning", 0.91),
                iq("code_review", 0.89),
                iq("translation", 0.90),
                iq("summarization", 0.88),
                iq("classification", 0.87),
                iq("extraction", 0.88),
            ],
        },
        ModelProfile {
            model_id: "gpt-4o-mini".into(),
            provider: "openai".into(),
            tier: "economy".into(),
            context_window: 128_000,
            input_cost_per_m: 0.15,
            output_cost_per_m: 0.60,
            supports_vision: true,
            supports_tools: true,
            intents: vec![
                iq("code_generation", 0.74),
                iq("reasoning", 0.70),
                iq("code_review", 0.71),
                iq("translation", 0.78),
                iq("summarization", 0.79),
                iq("classification", 0.80),
                iq("extraction", 0.79),
            ],
        },
        // ── Google ──────────────────────────────────────────────
        ModelProfile {
            model_id: "gemini-2.5-flash".into(),
            provider: "google".into(),
            tier: "economy".into(),
            context_window: 1_000_000,
            input_cost_per_m: 0.15,
            output_cost_per_m: 0.60,
            supports_vision: true,
            supports_tools: true,
            intents: vec![
                iq("code_generation", 0.78),
                iq("reasoning", 0.80),
                iq("code_review", 0.74),
                iq("translation", 0.82),
                iq("summarization", 0.83),
                iq("classification", 0.81),
                iq("extraction", 0.80),
            ],
        },
        ModelProfile {
            model_id: "gemini-2.5-pro".into(),
            provider: "google".into(),
            tier: "frontier".into(),
            context_window: 1_000_000,
            input_cost_per_m: 1.25,
            output_cost_per_m: 10.0,
            supports_vision: true,
            supports_tools: true,
            intents: vec![
                iq("code_generation", 0.93),
                iq("reasoning", 0.94),
                iq("code_review", 0.90),
                iq("translation", 0.89),
                iq("summarization", 0.88),
                iq("classification", 0.87),
                iq("extraction", 0.88),
            ],
        },
        // ── DeepSeek ────────────────────────────────────────────
        ModelProfile {
            model_id: "deepseek-v3".into(),
            provider: "deepseek".into(),
            tier: "economy".into(),
            context_window: 64_000,
            input_cost_per_m: 0.27,
            output_cost_per_m: 1.10,
            supports_vision: false,
            supports_tools: true,
            intents: vec![
                iq("code_generation", 0.88),
                iq("reasoning", 0.85),
                iq("code_review", 0.82),
                iq("translation", 0.72),
                iq("summarization", 0.75),
                iq("classification", 0.74),
                iq("extraction", 0.76),
            ],
        },
        ModelProfile {
            model_id: "deepseek-r1".into(),
            provider: "deepseek".into(),
            tier: "performance".into(),
            context_window: 64_000,
            input_cost_per_m: 0.55,
            output_cost_per_m: 2.19,
            supports_vision: false,
            supports_tools: false,
            intents: vec![
                iq("code_generation", 0.90),
                iq("reasoning", 0.93),
                iq("code_review", 0.86),
                iq("translation", 0.70),
                iq("summarization", 0.73),
                iq("classification", 0.72),
                iq("extraction", 0.74),
            ],
        },
    ]
}

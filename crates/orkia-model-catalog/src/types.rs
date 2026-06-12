// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProfile {
    pub model_id: String,
    pub provider: String,
    pub tier: String,
    pub context_window: i32,
    pub input_cost_per_m: f64,
    pub output_cost_per_m: f64,
    pub supports_vision: bool,
    pub supports_tools: bool,
    pub intents: Vec<IntentQuality>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentQuality {
    pub intent: String,
    pub quality_score: f32,
}

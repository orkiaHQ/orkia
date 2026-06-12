// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Typed shape of the `/v1/forge/build` server response.
//!
//! This module owns the deserialized representation only — the
//! HTTP transit lives in the proprietary cloud client. Splitting the type
//! into its own module lets the `write` and `manifest` modules
//! consume the typed payload without dragging in any HTTP machinery.

use chrono::{DateTime, Utc};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct BuildResponse {
    pub build_id: String,
    pub builder_version: String,
    pub model: String,
    pub files: GeneratedFiles,
    pub manifest_overrides: serde_json::Value,
    pub usage: ServerUsage,
}

#[derive(Debug, Deserialize)]
pub struct GeneratedFiles {
    pub html: String,
    pub css: String,
    pub js: String,
    pub icon_svg: String,
}

#[derive(Debug, Deserialize)]
pub struct ServerUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub retries: u32,
    pub duration_ms: u32,
    // Tolerated but not yet surfaced to callers; the next CLI version
    // will render these next to the "build complete" line. Keeping
    // them in the wire shape so adding the renderer is purely
    // additive.
    #[allow(dead_code)]
    pub remaining_quota: u32,
    #[allow(dead_code)]
    pub quota_reset_at: DateTime<Utc>,
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Capture per-turn final assistant responses from Orkia-managed
//!
//! Lifecycle:
//!
//! 1. The journal listener observes a `Stop` envelope from a known
//!    provider (claude / codex / gemini).
//! 2. It calls [`FinalResponseService::on_stop`] with the extracted
//!    `ExtractionContext`.
//! 3. The service spawns a background task that reads the provider's
//!    own transcript, extracts the assistant text, persists it under
//!    the run-dir, and emits a second journal envelope tagged
//!    `event = "AgentFinalResponse"` carrying the path + sha + preview.
//! 4. In-process subscribers registered via
//!    [`FinalResponseSource::subscribe`] receive the typed
//!    [`FinalResponseEvent`].

#![cfg_attr(not(test), deny(warnings))]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]

pub mod extractor;
pub mod extractors;
pub mod service;
pub mod storage;

pub use extractor::{ExtractionContext, ExtractionError, TranscriptExtractor};
pub use extractors::{ClaudeExtractor, CodexExtractor, GeminiExtractor};
pub use service::{FinalResponseService, NativePublishRequest};

// Re-export the public surface from `orkia-shell-types` so downstream
// consumers can `use orkia_final_response::{FinalResponseEvent,
// FinalResponseSource}` without naming the types crate directly.
pub use orkia_shell_types::{FinalResponseCallback, FinalResponseEvent, FinalResponseSource};

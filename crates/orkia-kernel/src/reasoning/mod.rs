// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! The reasoning subsystem, gated behind the Intelligence handle. Nothing here
//! is constructed unless the gate is open.

pub mod audit;
pub mod backfill;
pub mod consumer;
pub mod enrich;
pub mod scope;
pub mod scrub;
pub mod sync;

pub use audit::ReasoningAudit;
pub use backfill::{BackfillSyncConfig, backfill_sync};
pub use consumer::{CaptureScope, ReasoningConsumer};
pub use enrich::enrich_system_prompt;
pub use scope::{JobScope, JobScopes, new_job_scopes, scope_for};
pub use sync::{SYNC_INTERVAL, SyncConfig, SyncWorker};

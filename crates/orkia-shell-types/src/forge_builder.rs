// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Trait for the thing that turns an RFC into a Forge app on disk.
//!
//! V0 shipped a single sync implementation in
//! `orkia-builtin::forge::ScaffoldBuilder` (pure filesystem work). V1 adds
//! the proprietary cloud Forge client which calls a remote builder
//! service over HTTP and is therefore inherently async. To accommodate
//! both, the trait is now async — `ScaffoldBuilder::build` is still
//! sync internally but exposed through an `async fn` shim so callers
//! don't have to branch.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use orkia_forge_types::ForgeManifest;
use orkia_rfc_core::RfcRecord;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Usage report returned by `/v1/forge/usage` and surfaced by
/// `orkia app usage`. Lives here (rather than in the proprietary
/// cloud client) so the public shell can render it without
/// importing the proprietary crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageReport {
    pub plan: String,
    pub month_used: u32,
    pub month_limit: u32,
    pub hour_used: u32,
    pub hour_limit: u32,
    pub reset_at: DateTime<Utc>,
    pub recent: Vec<RecentBuild>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentBuild {
    pub id: String,
    pub success: bool,
    pub failure_reason: Option<String>,
    pub duration_ms: i32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct BuildOutcome {
    pub manifest: ForgeManifest,
    pub files_written: Vec<PathBuf>,
    pub duration: Duration,
    pub builder_version: String,
}

#[derive(Debug, Error)]
pub enum BuilderError {
    #[error("RFC invalid: {0}")]
    InvalidRfc(String),

    #[error("app `{name}` already exists; pass --force to overwrite")]
    AppExists { name: String },

    /// `--rerun` was passed but the RFC content hash is identical to the
    /// before burning a quota slot on a guaranteed-identical rebuild.
    #[error("RFC unchanged since last build; pass --yes to rebuild anyway")]
    RfcUnchanged,

    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),

    #[error("manifest error: {0}")]
    Manifest(String),

    #[error("generation failed after {retries} retries: {message}")]
    GenerationFailed { retries: u32, message: String },

    /// V1: backend rejected the request — user must run `orkia login`.
    #[error("not authenticated — run: orkia login")]
    AuthRequired,

    /// V1: user has exceeded their monthly plan quota.
    #[error("monthly quota exceeded for {plan} plan (resets at {reset_at})")]
    QuotaExceeded {
        plan: String,
        reset_at: DateTime<Utc>,
    },

    /// V1: hourly rate limit hit (free tier: 10/hr).
    #[error("rate limit hit, retry at {reset_at}")]
    RateLimit { reset_at: DateTime<Utc> },

    /// V1: network failure reaching the backend.
    #[error("network error: {0}")]
    Network(String),

    /// V1: backend returned 5xx.
    #[error("server error — try again or check status.orkia.dev")]
    ServerError,

    /// V1: backend returned an unexpected status code we don't have a
    /// dedicated mapping for. Surfaces the raw code so users can report it.
    #[error("unexpected response status: {0}")]
    UnexpectedStatus(u16),

    /// The Forge implementation is not available in this build (e.g.,
    /// OSS shell without a wired backend, or the offline scaffold path
    /// asked about usage).
    #[error("Forge unavailable: {reason}")]
    Unavailable { reason: String },
}

#[async_trait]
pub trait ForgeBuilder: Send + Sync {
    /// Materialize `rfc` into `target_dir`. The dir is expected to either not
    /// exist yet, or be empty (callers gate `--force` themselves).
    async fn build(&self, rfc: &RfcRecord, target_dir: &Path)
    -> Result<BuildOutcome, BuilderError>;

    /// Fetch the current usage quota for the authenticated workspace.
    /// Returns [`BuilderError::Unavailable`] for impls that don't
    /// surface a quota (e.g., the offline scaffold builder).
    async fn usage(&self) -> Result<UsageReport, BuilderError>;
}

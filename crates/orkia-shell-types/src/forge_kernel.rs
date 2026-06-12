// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Wire contract for the `kernel.v1.forge.*` RPCs.
//!
//! In the single-shell architecture the proprietary Forge HTTP client
//! moves into the `orkia-kernel` daemon: the OSS shell never links the
//! cloud client. The shell owns the local build mechanics (RFC hashing,
//! `write_build`, manifest merge — all in the public `orkia-forge-build`
//! crate) and asks the kernel only to perform the **authenticated HTTP
//! call** to the Forge backend.
//!
//! The shell sends the already-rendered RFC wire form plus the bearer
//! token and backend base URL it resolved itself; the kernel is a pure
//! relay that never reads the keychain or the shell's config. On success
//! it returns the backend's raw build payload (a [`serde_json::Value`])
//! which the shell deserializes into `orkia_forge_build::BuildResponse`
//! and materializes to disk. The kernel never touches the user's
//! filesystem.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::forge_builder::UsageReport;

/// JSON-RPC method names for the Forge RPCs. Single source of truth
/// shared by the kernel server dispatch and the shell-side proxy.
pub const METHOD_FORGE_BUILD: &str = "kernel.v1.forge.build";
pub const METHOD_FORGE_USAGE: &str = "kernel.v1.forge.usage";

/// Params for [`METHOD_FORGE_BUILD`]. The shell renders the RFC and
/// computes the hashes itself (via `orkia-forge-build`), so the kernel
/// only forwards bytes to `{api_url}/v1/forge/build`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeBuildRequest {
    /// Backend base URL the shell resolved (`resolve_backend_url`).
    pub api_url: String,
    /// Bearer the shell read from its session; the kernel holds no auth.
    pub bearer: String,
    pub rfc_content: String,
    pub rfc_hash: String,
    pub client_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_app_hash: Option<String>,
}

/// Params for [`METHOD_FORGE_USAGE`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeUsageRequest {
    pub api_url: String,
    pub bearer: String,
}

/// A serializable mirror of the HTTP-derived `BuilderError` variants.
/// The shell maps these back into `BuilderError` so the user-facing
/// behaviour is identical to the in-process cloud client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ForgeWireError {
    AuthRequired,
    QuotaExceeded {
        plan: String,
        reset_at: DateTime<Utc>,
    },
    RateLimit {
        reset_at: DateTime<Utc>,
    },
    InvalidRfc {
        message: String,
    },
    GenerationFailed {
        retries: u32,
        message: String,
    },
    ServerError,
    UnexpectedStatus {
        code: u16,
    },
    Network {
        message: String,
    },
}

/// Result of [`METHOD_FORGE_BUILD`]. `Built.build` is the backend's raw
/// build payload, deserialized shell-side into `BuildResponse`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ForgeBuildResponse {
    Built { build: Value },
    Failed { error: ForgeWireError },
}

/// Result of [`METHOD_FORGE_USAGE`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ForgeUsageResponse {
    Usage { report: UsageReport },
    Failed { error: ForgeWireError },
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! [`KernelForgeProxy`] — the OSS [`ForgeBuilder`] for the single shell.
//!
//! The proprietary cloud Forge client never links into `orkia`. Instead
//! the proxy keeps the deterministic local half (RFC rendering, hashing,
//! `write_build`, manifest load — all in the public `orkia-forge-build`
//! crate) and asks the `orkia-kernel` daemon to perform the authenticated
//! `/v1/forge/*` HTTP call. The shell reads the bearer from its own
//! session and passes it per-request; the kernel never touches the
//! keychain or the user's filesystem.
//!
//! This mirrors `orkia-pipeline-proxy`: the kernel decides/relays, the
//! shell owns local I/O. Attached only when the `ForgeBuild` capability is
//! unlocked **and** a kernel is reachable; otherwise the shell stays on
//! `NoopForgeBuilder` and Forge returns the premium-required message
//! (fail-closed).

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use orkia_auth::AuthProvider;
use orkia_forge_build::{
    BuildResponse, load_previous_hash, render_rfc_for_wire, sha256_hex, write_build,
};
use orkia_forge_types::ForgeManifest;
use orkia_rfc_core::RfcRecord;
use orkia_shell_types::{
    BuildOutcome, BuilderError, ForgeBuildRequest, ForgeBuildResponse, ForgeBuilder,
    ForgeUsageRequest, ForgeUsageResponse, ForgeWireError, KernelRpc, KernelRpcError, UsageReport,
};

/// Forge builder that relays the HTTP call through the kernel.
pub struct KernelForgeProxy {
    kernel: Arc<dyn KernelRpc>,
    auth: Arc<dyn AuthProvider>,
    api_url: String,
}

impl KernelForgeProxy {
    pub fn new(kernel: Arc<dyn KernelRpc>, auth: Arc<dyn AuthProvider>, api_url: String) -> Self {
        Self {
            kernel,
            auth,
            api_url,
        }
    }

    fn bearer(&self) -> Result<String, BuilderError> {
        self.auth.bearer().ok_or(BuilderError::AuthRequired)
    }
}

#[async_trait]
impl ForgeBuilder for KernelForgeProxy {
    async fn build(
        &self,
        rfc: &RfcRecord,
        target_dir: &Path,
    ) -> Result<BuildOutcome, BuilderError> {
        let started = Instant::now();
        let bearer = self.bearer()?;

        let forge_block = rfc
            .fm
            .forge
            .as_ref()
            .ok_or_else(|| BuilderError::InvalidRfc("[forge] block missing".into()))?;
        if rfc.fm.kind.as_deref() != Some("forge-app") {
            return Err(BuilderError::InvalidRfc(
                "RFC kind must be \"forge-app\"".into(),
            ));
        }

        let rfc_content = render_rfc_for_wire(rfc);
        let rfc_hash = sha256_hex(rfc_content.as_bytes());
        let req = ForgeBuildRequest {
            api_url: self.api_url.clone(),
            bearer,
            rfc_content: rfc_content.clone(),
            rfc_hash: rfc_hash.clone(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            previous_app_hash: load_previous_hash(target_dir),
        };

        let kernel = self.kernel.clone();
        let resp = blocking(move || kernel.forge_build(req))
            .await
            .map_err(rpc_to_builder)?;
        let build = match resp {
            ForgeBuildResponse::Built { build } => build,
            ForgeBuildResponse::Failed { error } => return Err(wire_to_builder(error)),
        };

        let body: BuildResponse = serde_json::from_value(build)
            .map_err(|e| BuilderError::Manifest(format!("response parse: {e}")))?;
        let files_written = write_build(target_dir, forge_block, &rfc_content, &body, &rfc_hash)?;
        let manifest = ForgeManifest::load(&target_dir.join("manifest.toml"))
            .map_err(|e| BuilderError::Manifest(e.to_string()))?;
        Ok(BuildOutcome {
            manifest,
            files_written,
            duration: started.elapsed(),
            builder_version: body.builder_version,
        })
    }

    async fn usage(&self) -> Result<UsageReport, BuilderError> {
        let bearer = self.bearer()?;
        let req = ForgeUsageRequest {
            api_url: self.api_url.clone(),
            bearer,
        };
        let kernel = self.kernel.clone();
        let resp = blocking(move || kernel.forge_usage(req))
            .await
            .map_err(rpc_to_builder)?;
        match resp {
            ForgeUsageResponse::Usage { report } => Ok(report),
            ForgeUsageResponse::Failed { error } => Err(wire_to_builder(error)),
        }
    }
}

/// Run a blocking kernel RPC on the blocking pool, mapping a join failure
/// into a transport error. Mirrors `orkia-pipeline-proxy::run::blocking`.
async fn blocking<T, F>(f: F) -> Result<T, KernelRpcError>
where
    F: FnOnce() -> Result<T, KernelRpcError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r,
        Err(e) => Err(KernelRpcError::Io(format!("kernel rpc task: {e}"))),
    }
}

/// Map a transport error to a `BuilderError`. An unreachable kernel means
/// the premium relay isn't active → surface it as unavailable.
fn rpc_to_builder(e: KernelRpcError) -> BuilderError {
    match e {
        KernelRpcError::Unavailable(reason) => BuilderError::Unavailable { reason },
        other => BuilderError::Network(other.to_string()),
    }
}

/// Map the wire error back to the canonical `BuilderError` so the user
/// sees the identical message the in-process client produced.
fn wire_to_builder(e: ForgeWireError) -> BuilderError {
    match e {
        ForgeWireError::AuthRequired => BuilderError::AuthRequired,
        ForgeWireError::QuotaExceeded { plan, reset_at } => {
            BuilderError::QuotaExceeded { plan, reset_at }
        }
        ForgeWireError::RateLimit { reset_at } => BuilderError::RateLimit { reset_at },
        ForgeWireError::InvalidRfc { message } => BuilderError::InvalidRfc(message),
        ForgeWireError::GenerationFailed { retries, message } => {
            BuilderError::GenerationFailed { retries, message }
        }
        ForgeWireError::ServerError => BuilderError::ServerError,
        ForgeWireError::UnexpectedStatus { code } => BuilderError::UnexpectedStatus(code),
        ForgeWireError::Network { message } => BuilderError::Network(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn wire_quota_preserves_plan_and_reset() {
        let reset = Utc::now();
        let err = wire_to_builder(ForgeWireError::QuotaExceeded {
            plan: "team".into(),
            reset_at: reset,
        });
        match err {
            BuilderError::QuotaExceeded { plan, reset_at } => {
                assert_eq!(plan, "team");
                assert_eq!(reset_at, reset);
            }
            other => panic!("expected QuotaExceeded, got {other:?}"),
        }
    }

    #[test]
    fn wire_unexpected_status_preserves_code() {
        match wire_to_builder(ForgeWireError::UnexpectedStatus { code: 418 }) {
            BuilderError::UnexpectedStatus(code) => assert_eq!(code, 418),
            other => panic!("expected UnexpectedStatus, got {other:?}"),
        }
    }

    #[test]
    fn unreachable_kernel_maps_to_unavailable() {
        let err = rpc_to_builder(KernelRpcError::Unavailable("no daemon".into()));
        assert!(matches!(err, BuilderError::Unavailable { .. }));
    }
}

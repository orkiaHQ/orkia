// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `KernelRpc` — the trait surface the shell uses to talk to an
//! optional `orkia-kernel` daemon over local IPC.
//!
//! Defined here so `orkia-shell` consumes it without depending on the
//! concrete client crate. Implementations live in `orkia-kernel-client`
//! (talks to a Unix socket) and any test stub.
//!
//! All methods are synchronous to keep parity with [`IntentClassifier`],
//! which is called from inside the synchronous REPL classify path.
//! Implementations are expected to enforce their own timeouts and
//! return [`KernelRpcError::Timeout`] rather than block indefinitely.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::classifier::IntentGuess;
use crate::forge_kernel::{
    ForgeBuildRequest, ForgeBuildResponse, ForgeUsageRequest, ForgeUsageResponse,
};
use crate::native::{NativeCompletionRequest, NativeCompletionResponse};
use crate::dispatch_kernel::{
    DispatchAbortRequest, DispatchAbortResponse, DispatchAdvanceRequest, DispatchAdvanceResponse,
    DispatchAuthorizeRequest, DispatchAuthorizeResponse,
};
use crate::pipeline_kernel::{
    PipelineAbortRequest, PipelineAbortResponse, PipelineAdvanceRequest, PipelineAdvanceResponse,
    PipelineAuthorizeRequest, PipelineAuthorizeResponse,
};
use crate::seal_assembler::AssembleRequest;
use crate::seal_kernel::{SealAssembleResponse, SealVerifyRequest, SealVerifyResponse};

/// Status row returned by `kernel.v1.models.list`. Mirrors the
/// server-side `ModelStatus` so the shell can render without
/// schema duplication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelModelStatus {
    pub id: String,
    pub version: String,
    pub installed: bool,
    pub size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum KernelPullOutcome {
    Started { id: String, size_bytes: u64 },
    AlreadyInstalled { id: String },
    NotInRegistry { id: String },
    Unsupported,
    Error { message: String },
}

/// Outcome of `kernel.v1.benchmark`. Latencies in milliseconds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum KernelBenchmarkOutcome {
    Ran {
        rounds: u32,
        p50_ms: u64,
        p95_ms: u64,
        p99_ms: u64,
        errors: u32,
    },
    Unsupported,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelContributeStatus {
    pub granted: bool,
    pub kernel_id: String,
    pub journal_count: u64,
    pub posted_last_24h: u64,
    pub policy_disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_remaining_seconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum KernelContributeOutcome {
    Ok,
    PhraseMismatch,
    DisabledByPolicy,
    Unsupported,
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum KernelCancelOutcome {
    Cancelled { id: String },
    NotFound { id: String },
    Unsupported,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum KernelEvictOutcome {
    Evicted { id: String },
    Nothing,
    Unsupported,
}

/// Identity reported by the kernel during the handshake. The shell uses
/// `protocol` (a monotonic integer, bumped on every breaking wire change)
/// to gate optional features and logs `kernel` for support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelVersion {
    /// Wire-protocol revision the daemon speaks. Compatible iff both
    /// sides land within each other's [MIN, CURRENT] window.
    pub protocol: u32,
    /// Daemon build version (informational only).
    pub kernel: String,
    /// Lowest client-protocol revision the daemon will serve. The
    /// shell aborts handshake when `CURRENT_KERNEL_PROTOCOL` is below
    /// this. `None` from the wire means "no floor advertised."
    pub min_client: Option<u32>,
    /// Optional feature bag advertised by the daemon. Names are open
    /// strings; callers gate on `contains`.
    pub capabilities: Vec<String>,
}

/// Errors a `KernelRpc` implementation may surface. The classifier
/// path treats every variant identically — falling back to the
/// heuristic — but the variants exist so the shell can log meaningful
/// diagnostics in `$kernel` / `$whoami`.
#[derive(Debug, thiserror::Error)]
pub enum KernelRpcError {
    /// Socket missing, refused, or peer disappeared between calls.
    #[error("kernel unavailable: {0}")]
    Unavailable(String),
    /// The kernel did not respond within the caller's deadline.
    #[error("kernel timeout")]
    Timeout,
    /// Handshake failed or the response did not parse against the
    /// expected schema.
    #[error("kernel protocol error: {0}")]
    Protocol(String),
    /// Underlying I/O failure (socket read/write).
    #[error("kernel io error: {0}")]
    Io(String),
}

/// Local IPC contract between the shell and the kernel daemon. Held
/// behind `Arc<dyn KernelRpc>` in the shell so it can be swapped at
/// runtime when the user's plan changes without rebuilding the REPL.
///
/// Future revisions extend with `route`, `embed`, `compress`, model
/// download progress subscriptions, etc.
pub trait KernelRpc: Send + Sync + 'static {
    /// Identity of the remote kernel. Returned by the handshake at
    /// connection time and cached locally — never round-trips on
    /// subsequent calls.
    fn version(&self) -> KernelVersion;

    /// Classify a free-text line via the kernel. Implementations
    /// MUST return [`KernelRpcError::Timeout`] if the round-trip
    /// exceeds `timeout`; the shell uses this as a hard ceiling so a
    /// stuck kernel never stalls the REPL.
    fn classify_with_timeout(
        &self,
        line: &str,
        timeout: Duration,
    ) -> Result<IntentGuess, KernelRpcError>;

    /// Politely ask the kernel to wind down. Best-effort: the shell
    /// uses this on `$logout` so the daemon does not keep models
    /// resident after the user is no longer entitled. Errors are
    /// logged but never surfaced.
    fn shutdown(&self) -> Result<(), KernelRpcError>;

    fn list_models(&self) -> Result<Vec<KernelModelStatus>, KernelRpcError> {
        Err(KernelRpcError::Unavailable(
            "list_models not implemented by this client".into(),
        ))
    }

    fn pull_model(&self, id: &str) -> Result<KernelPullOutcome, KernelRpcError> {
        let _ = id;
        Err(KernelRpcError::Unavailable(
            "pull_model not implemented by this client".into(),
        ))
    }

    fn benchmark(&self, rounds: u32) -> Result<KernelBenchmarkOutcome, KernelRpcError> {
        let _ = rounds;
        Err(KernelRpcError::Unavailable(
            "benchmark not implemented by this client".into(),
        ))
    }

    fn contribute_status(&self) -> Result<KernelContributeStatus, KernelRpcError> {
        Err(KernelRpcError::Unavailable(
            "contribute_status not implemented by this client".into(),
        ))
    }

    fn contribute_set(
        &self,
        on: bool,
        phrase: Option<&str>,
    ) -> Result<KernelContributeOutcome, KernelRpcError> {
        let _ = (on, phrase);
        Err(KernelRpcError::Unavailable(
            "contribute_set not implemented by this client".into(),
        ))
    }

    fn contribute_purge(&self) -> Result<KernelContributeOutcome, KernelRpcError> {
        Err(KernelRpcError::Unavailable(
            "contribute_purge not implemented by this client".into(),
        ))
    }

    fn cancel_pull(&self, id: &str) -> Result<KernelCancelOutcome, KernelRpcError> {
        let _ = id;
        Err(KernelRpcError::Unavailable(
            "cancel_pull not implemented by this client".into(),
        ))
    }

    fn evict_loaded(&self) -> Result<KernelEvictOutcome, KernelRpcError> {
        Err(KernelRpcError::Unavailable(
            "evict_loaded not implemented by this client".into(),
        ))
    }

    /// `@a | @b`: ask the kernel to validate and authorize a pipeline of
    /// pre-resolved stages. The kernel holds the run state and returns the
    /// plan for stage 0 (or a refusal). Default Unavailable so stub clients
    /// keep compiling.
    fn pipeline_authorize(
        &self,
        req: PipelineAuthorizeRequest,
    ) -> Result<PipelineAuthorizeResponse, KernelRpcError> {
        let _ = req;
        Err(KernelRpcError::Unavailable(
            "pipeline_authorize not implemented by this client".into(),
        ))
    }

    /// `@a | @b`: report a completed stage's output (by file ref) and ask
    /// the kernel for the next stage or completion.
    fn pipeline_advance(
        &self,
        req: PipelineAdvanceRequest,
    ) -> Result<PipelineAdvanceResponse, KernelRpcError> {
        let _ = req;
        Err(KernelRpcError::Unavailable(
            "pipeline_advance not implemented by this client".into(),
        ))
    }

    /// `@a | @b`: tell the kernel a run was cancelled so it drops the
    /// in-memory run state.
    fn pipeline_abort(
        &self,
        req: PipelineAbortRequest,
    ) -> Result<PipelineAbortResponse, KernelRpcError> {
        let _ = req;
        Err(KernelRpcError::Unavailable(
            "pipeline_abort not implemented by this client".into(),
        ))
    }

    /// RFC dispatch (`SPEC-ORKIA-RFC-DISPATCH`): ask the kernel to validate
    /// a declarative DAG of pre-resolved tasks and open a run. The kernel
    /// holds the run state and returns the first wave of ready task plans
    /// (or a refusal). Default Unavailable so stub clients keep compiling.
    fn dispatch_authorize(
        &self,
        req: DispatchAuthorizeRequest,
    ) -> Result<DispatchAuthorizeResponse, KernelRpcError> {
        let _ = req;
        Err(KernelRpcError::Unavailable(
            "dispatch_authorize not implemented by this client".into(),
        ))
    }

    /// RFC dispatch: report one task's outcome (by file ref) and ask the
    /// kernel for the next wave of ready tasks or a terminal verdict.
    fn dispatch_advance(
        &self,
        req: DispatchAdvanceRequest,
    ) -> Result<DispatchAdvanceResponse, KernelRpcError> {
        let _ = req;
        Err(KernelRpcError::Unavailable(
            "dispatch_advance not implemented by this client".into(),
        ))
    }

    /// RFC dispatch: tell the kernel a run was cancelled so it drops the
    /// in-memory run state (idempotent).
    fn dispatch_abort(
        &self,
        req: DispatchAbortRequest,
    ) -> Result<DispatchAbortResponse, KernelRpcError> {
        let _ = req;
        Err(KernelRpcError::Unavailable(
            "dispatch_abort not implemented by this client".into(),
        ))
    }

    /// Forge: relay an authenticated `/v1/forge/build` call. The shell
    /// renders the RFC + computes hashes itself and materializes the
    /// result; the kernel only performs the HTTP request. Default
    /// Unavailable so stub clients keep compiling.
    fn forge_build(&self, req: ForgeBuildRequest) -> Result<ForgeBuildResponse, KernelRpcError> {
        let _ = req;
        Err(KernelRpcError::Unavailable(
            "forge_build not implemented by this client".into(),
        ))
    }

    /// Forge: relay an authenticated `/v1/forge/usage` call.
    fn forge_usage(&self, req: ForgeUsageRequest) -> Result<ForgeUsageResponse, KernelRpcError> {
        let _ = req;
        Err(KernelRpcError::Unavailable(
            "forge_usage not implemented by this client".into(),
        ))
    }

    /// SEAL: assemble a per-RFC SEAL v1 document kernel-side (the signing
    /// crate is premium and never links into the OSS shell). The kernel
    /// reads the local audit ledgers under `req.data_dir` and writes the
    /// document under `<data_dir>/seal-v1/`.
    fn seal_assemble(&self, req: AssembleRequest) -> Result<SealAssembleResponse, KernelRpcError> {
        let _ = req;
        Err(KernelRpcError::Unavailable(
            "seal_assemble not implemented by this client".into(),
        ))
    }

    /// SEAL: verify a previously assembled SEAL v1 document kernel-side.
    fn seal_verify(&self, req: SealVerifyRequest) -> Result<SealVerifyResponse, KernelRpcError> {
        let _ = req;
        Err(KernelRpcError::Unavailable(
            "seal_verify not implemented by this client".into(),
        ))
    }

    /// Native runtime: relay one model completion to the configured
    /// provider (`kernel.v1.llm.complete`). The shell owns the agent
    /// loop and the tools; the kernel only performs the HTTP call.
    /// Default Unavailable so stub clients keep compiling — the shell
    /// refuses the native session on this error (fail-closed, never a
    /// vendor fallback).
    fn llm_complete(
        &self,
        req: NativeCompletionRequest,
    ) -> Result<NativeCompletionResponse, KernelRpcError> {
        let _ = req;
        Err(KernelRpcError::Unavailable(
            "llm_complete not implemented by this client".into(),
        ))
    }
}

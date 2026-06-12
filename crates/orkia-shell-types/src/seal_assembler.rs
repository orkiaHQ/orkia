// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! The assembler's implementation depends on the Apache-2.0 runtime
//! sub-workspace (`orkia-audit`, `orkia-governance`), which lives outside
//! the public Elastic-2.0 workspace. To keep the public workspace self-contained
//! and free of any path reference into the private tree, the shell talks
//! to the assembler through the [`RfcSealAssembler`] trait and the plain
//! data types below. The OSS build leaves the assembler unwired (`None`);
//! the proprietary distribution injects the concrete implementation.
//!
//! This mirrors the [`crate::ForgeBuilder`] injection boundary.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use orkia_rfc_core::RfcId;
use serde::{Deserialize, Serialize};

/// Request for [`RfcSealAssembler::assemble`].
///
/// Serializable so the OSS shell can relay it to the `orkia-kernel`
/// daemon (`kernel.v1.seal.assemble`) — the assembler impl links the
/// private runtime sub-workspace and lives kernel-side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssembleRequest {
    /// RFC whose tagged events to collect.
    pub rfc_id: RfcId,
    /// Root of the local data directory (typically `~/.orkia`).
    pub data_dir: PathBuf,
    /// Why the RFC is being closed. Determines the footer event type
    /// the assembler appends as the final entry.
    pub closure: ClosureReason,
}

/// Reason the RFC closed — drives the footer event the assembler emits
/// as the final entry of the document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClosureReason {
    Completed,
    Abandoned { reason: String },
}

/// Outcome of a successful assembly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssembleResult {
    /// Absolute path where the assembled document was written.
    pub output_path: PathBuf,
    /// Number of events embedded in the document (excludes header/footer).
    pub event_count: usize,
    /// Bytes written to disk.
    pub bytes_written: usize,
}

/// Verification outcome — distinguishes structural validity (well-formed
/// JSONL, recomputed chain matches, signature verifies) from semantic
/// commentary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VerifyOutcome {
    Valid {
        event_count: usize,
        /// SHA-256 head of the SEAL hash chain (the last event's `event_hash`).
        chain_head_hash: String,
    },
    Invalid {
        reason: String,
    },
}

impl VerifyOutcome {
    pub fn is_valid(&self) -> bool {
        matches!(self, Self::Valid { .. })
    }
}

/// Opaque assembler error. The shell only renders the `Display` of this
/// type, so the public boundary keeps a single message-carrying variant
/// rather than re-exporting the implementation's internal error taxonomy.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SealAssemblerError(pub String);

/// The per-RFC SEAL v1 assembler, behind a trait so the public shell
/// never links the runtime sub-workspace. Injected by the proprietary
/// binary; absent (`None`) in OSS builds.
#[async_trait]
pub trait RfcSealAssembler: Send + Sync {
    /// Build a SEAL v1 document for one RFC and write it under
    /// `<data_dir>/seal-v1/`.
    async fn assemble(
        &self,
        request: AssembleRequest,
    ) -> Result<AssembleResult, SealAssemblerError>;

    /// Verify a previously assembled document at `document_path`.
    async fn verify(&self, document_path: &Path) -> Result<VerifyOutcome, SealAssemblerError>;
}

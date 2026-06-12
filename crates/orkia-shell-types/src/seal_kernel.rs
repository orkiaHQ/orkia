// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Wire contract for the `kernel.v1.seal.*` RPCs.
//!
//! The per-RFC SEAL v1 assembler signs documents with code that lives in
//! the private runtime sub-workspace (`orkia-audit`, `orkia-governance`),
//! which must never link into the OSS shell. Unlike Forge this is pure
//! local CPU + filesystem work (collect audit ledgers → canonicalize →
//! hash-chain → ECDSA-P256 sign → write), but the *signing crate* is
//! premium, so the whole assemble/verify operation runs kernel-side and
//! the OSS shell drives it over this RPC.
//!
//! The kernel reads the local audit ledgers under `data_dir` and writes
//! the assembled document under `<data_dir>/seal-v1/` — the shell passes
//! `data_dir` per request (stateless; the kernel holds no path of its
//! own). This is the same posture the journal RPCs already take when the
//! kernel reads capped envelope files. No PTY is involved.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::seal_assembler::{AssembleResult, VerifyOutcome};

/// JSON-RPC method names for the SEAL RPCs. Single source of truth shared
/// by the kernel server dispatch and the shell-side proxy.
pub const METHOD_SEAL_ASSEMBLE: &str = "kernel.v1.seal.assemble";
pub const METHOD_SEAL_VERIFY: &str = "kernel.v1.seal.verify";

/// Params for [`METHOD_SEAL_VERIFY`]: the path of a previously assembled
/// document the kernel re-reads and re-verifies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealVerifyRequest {
    pub document_path: PathBuf,
}

/// Result of [`METHOD_SEAL_ASSEMBLE`]. `Failed.message` carries the
/// assembler's opaque `Display`, which the shell wraps back into
/// `SealAssemblerError` so the user sees the identical message.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SealAssembleResponse {
    Assembled { result: AssembleResult },
    Failed { message: String },
}

/// Result of [`METHOD_SEAL_VERIFY`]. `Verified` carries the verifier's
/// own `VerifyOutcome` (which already distinguishes `Valid`/`Invalid`);
/// `Failed` means the verify *operation* itself could not run (e.g. the
/// document path was unreadable).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SealVerifyResponse {
    Verified { outcome: VerifyOutcome },
    Failed { message: String },
}

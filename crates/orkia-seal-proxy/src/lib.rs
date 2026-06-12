// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! [`KernelSealProxy`] — the OSS [`RfcSealAssembler`] for the single shell.
//!
//! The per-RFC SEAL v1 assembler signs with code from the private runtime
//! sub-workspace (`orkia-audit`/`orkia-governance`) that never links into
//! `orkia`. Unlike Forge this is pure local CPU + filesystem work, but the
//! *signing crate* is premium, so the whole assemble/verify runs in the
//! `orkia-kernel` daemon and this proxy just relays the call over
//! `kernel.v1.seal.*`.
//!
//! Attached only when [`Capability::SealAuditExtended`] is unlocked **and**
//! a kernel is reachable; otherwise the shell leaves the assembler unwired
//! and `rfc complete` / `orkia rfc seal` report "not wired" (fail-closed).
//! The proxy carries no auth: assembly needs no bearer (it is
//! local), so the kernel reads the audit ledgers under the `data_dir` the
//! shell passes per request and holds no path state of its own.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use orkia_shell_types::{
    AssembleRequest, AssembleResult, KernelRpc, KernelRpcError, RfcSealAssembler,
    SealAssembleResponse, SealAssemblerError, SealVerifyRequest, SealVerifyResponse, VerifyOutcome,
};

/// SEAL assembler that relays assembly/verification through the kernel.
pub struct KernelSealProxy {
    kernel: Arc<dyn KernelRpc>,
}

impl KernelSealProxy {
    pub fn new(kernel: Arc<dyn KernelRpc>) -> Self {
        Self { kernel }
    }
}

#[async_trait]
impl RfcSealAssembler for KernelSealProxy {
    async fn assemble(
        &self,
        request: AssembleRequest,
    ) -> Result<AssembleResult, SealAssemblerError> {
        let kernel = self.kernel.clone();
        let resp = blocking(move || kernel.seal_assemble(request))
            .await
            .map_err(rpc_to_seal)?;
        match resp {
            SealAssembleResponse::Assembled { result } => Ok(result),
            SealAssembleResponse::Failed { message } => Err(SealAssemblerError(message)),
        }
    }

    async fn verify(&self, document_path: &Path) -> Result<VerifyOutcome, SealAssemblerError> {
        let req = SealVerifyRequest {
            document_path: document_path.to_path_buf(),
        };
        let kernel = self.kernel.clone();
        let resp = blocking(move || kernel.seal_verify(req))
            .await
            .map_err(rpc_to_seal)?;
        match resp {
            SealVerifyResponse::Verified { outcome } => Ok(outcome),
            SealVerifyResponse::Failed { message } => Err(SealAssemblerError(message)),
        }
    }
}

/// Run a blocking kernel RPC on the blocking pool, mapping a join failure
/// into a transport error. Mirrors `orkia-forge-proxy::blocking`.
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

/// Map a transport error to a `SealAssemblerError`. The boundary error is
/// opaque (a single message), so an unreachable kernel and a wire failure
/// both surface as a clear, message-carrying error the shell renders.
fn rpc_to_seal(e: KernelRpcError) -> SealAssemblerError {
    match e {
        KernelRpcError::Unavailable(reason) => {
            SealAssemblerError(format!("kernel unavailable: {reason}"))
        }
        other => SealAssemblerError(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A `KernelRpc` whose seal methods are the trait defaults (Unavailable).
    struct UnwiredKernel;
    impl KernelRpc for UnwiredKernel {
        fn version(&self) -> orkia_shell_types::KernelVersion {
            orkia_shell_types::KernelVersion {
                protocol: 1,
                kernel: "test".into(),
                min_client: None,
                capabilities: Vec::new(),
            }
        }
        fn classify_with_timeout(
            &self,
            _line: &str,
            _timeout: Duration,
        ) -> Result<orkia_shell_types::IntentGuess, KernelRpcError> {
            Err(KernelRpcError::Unavailable("test".into()))
        }
        fn shutdown(&self) -> Result<(), KernelRpcError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn unreachable_kernel_maps_to_error() {
        let proxy = KernelSealProxy::new(Arc::new(UnwiredKernel));
        let err = proxy
            .verify(Path::new("/tmp/none.seal.jsonl"))
            .await
            .unwrap_err();
        assert!(err.0.contains("kernel unavailable"));
    }

    #[test]
    fn rpc_unavailable_is_labelled() {
        let err = rpc_to_seal(KernelRpcError::Unavailable("no daemon".into()));
        assert!(err.0.contains("kernel unavailable"));
        assert!(err.0.contains("no daemon"));
    }
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! Local model lifecycle — a thin client over the kernel daemon's
//! `models.{list,pull,cancel}` RPCs. The daemon owns the model-manager
//! (HTTP-Range resume, sha256 verify); this crate holds **no** download logic
//! and never depends on the proprietary model-manager directly.

use std::sync::Arc;

use orkia_shell_types::{
    KernelCancelOutcome, KernelModelStatus, KernelPullOutcome, KernelRpc, KernelRpcError,
};

/// Errors surfaced by the model-lifecycle facade.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    /// No kernel daemon is reachable — model management is unavailable.
    #[error("no kernel daemon reachable")]
    NoKernel,
    /// The daemon returned an RPC error.
    #[error("kernel rpc: {0}")]
    Rpc(#[from] KernelRpcError),
}

/// Facade over local model management. Delegates every call to the daemon.
#[derive(Clone)]
pub struct Models {
    rpc: Option<Arc<dyn KernelRpc>>,
}

impl Models {
    pub fn new(rpc: Option<Arc<dyn KernelRpc>>) -> Self {
        Self { rpc }
    }

    /// List installed/available models and their status.
    pub fn list(&self) -> Result<Vec<KernelModelStatus>, ModelError> {
        Ok(self.rpc()?.list_models()?)
    }

    /// Start (or resume) a model download. The daemon resumes from the
    /// `.part` file on the disk it owns — there is no restart-from-zero.
    pub fn pull(&self, id: &str) -> Result<KernelPullOutcome, ModelError> {
        Ok(self.rpc()?.pull_model(id)?)
    }

    /// Cancel an in-flight download.
    pub fn cancel(&self, id: &str) -> Result<KernelCancelOutcome, ModelError> {
        Ok(self.rpc()?.cancel_pull(id)?)
    }

    /// Per-id status, derived from the list (the daemon has no separate
    /// per-id status RPC; `list` already carries `installed`/`size_bytes`).
    pub fn status(&self, id: &str) -> Result<Option<KernelModelStatus>, ModelError> {
        Ok(self.list()?.into_iter().find(|m| m.id == id))
    }

    fn rpc(&self) -> Result<&Arc<dyn KernelRpc>, ModelError> {
        self.rpc.as_ref().ok_or(ModelError::NoKernel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_kernel_is_an_error_not_a_panic() {
        let m = Models::new(None);
        assert!(matches!(m.list(), Err(ModelError::NoKernel)));
        assert!(matches!(m.pull("x"), Err(ModelError::NoKernel)));
        assert!(matches!(m.status("x"), Err(ModelError::NoKernel)));
    }
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! Classification wrapper. Thin pass-through to the kernel daemon over
//! `orkia-kernel-client`; exists so callers go through the one Intelligence
//! gate rather than reaching for the RPC client directly.

use std::sync::Arc;
use std::time::Duration;

use orkia_shell_types::{IntentGuess, KernelRpc, KernelRpcError};

/// Default classification budget. Matches the REPL's per-keystroke headroom.
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(40);

/// Wraps an optional kernel RPC handle for classification.
#[derive(Clone)]
pub struct Classifier {
    rpc: Option<Arc<dyn KernelRpc>>,
}

impl Classifier {
    pub fn new(rpc: Option<Arc<dyn KernelRpc>>) -> Self {
        Self { rpc }
    }

    /// Classify a line. Returns `None` when no kernel is reachable (caller
    /// falls back to the in-process heuristic) or the RPC errors.
    pub fn classify(&self, line: &str) -> Option<IntentGuess> {
        self.classify_with_timeout(line, DEFAULT_TIMEOUT)
    }

    pub fn classify_with_timeout(&self, line: &str, timeout: Duration) -> Option<IntentGuess> {
        let rpc = self.rpc.as_ref()?;
        match rpc.classify_with_timeout(line, timeout) {
            Ok(g) => Some(g),
            Err(e) => {
                log_rpc_err("classify", &e);
                None
            }
        }
    }
}

pub(crate) fn log_rpc_err(op: &str, e: &KernelRpcError) {
    tracing::debug!(op, error = %e, "kernel rpc unavailable; falling back");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_rpc_returns_none() {
        let c = Classifier::new(None);
        assert!(c.classify("@faye hi").is_none());
    }
}

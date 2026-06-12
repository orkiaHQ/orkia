// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Attach the kernel-backed Forge builder when the plan unlocks it.
//!
//! Forge is gated on [`Capability::ForgeBuild`] (solo-pro and up) plus a
//! reachable `orkia-kernel` daemon — the same dual condition as the
//! pipeline coordinator. The kernel relays the `/v1/forge/*` HTTP call;
//! the shell keeps the local build mechanics. Absent either condition the
//! caller falls back to `NoopForgeBuilder`, so `orkia app build` returns
//! the premium-required message (fail-closed).

use std::sync::Arc;

use orkia_auth::AuthProvider;
use orkia_capabilities::{Capability, CapabilityResolver};
use orkia_forge_proxy::KernelForgeProxy;
use orkia_shell_types::ForgeBuilder;
use orkia_shell_types::backend::{DEFAULT_BACKEND_URL, resolve_backend_url};

/// Build the kernel-backed Forge builder, or `None` when the capability
/// is locked or no daemon is reachable.
pub(crate) fn build(
    resolver: &Arc<dyn CapabilityResolver>,
    auth: &Arc<dyn AuthProvider>,
) -> Option<Arc<dyn ForgeBuilder>> {
    if !resolver.current().has(Capability::ForgeBuild) {
        return None;
    }
    let kernel = orkia_kernel_client::discover()?;
    let api_url = resolve_backend_url(None).unwrap_or_else(|_| DEFAULT_BACKEND_URL.to_string());
    Some(Arc::new(KernelForgeProxy::new(
        kernel,
        auth.clone(),
        api_url,
    )))
}

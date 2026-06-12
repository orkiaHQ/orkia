// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Attach the kernel-backed SEAL v1 assembler when the plan unlocks it.
//!
//! Per-RFC SEAL assembly is gated on [`Capability::SealAuditExtended`]
//! plus a reachable `orkia-kernel` daemon — the same dual condition as the
//! pipeline coordinator and Forge. The premium signing runs kernel-side;
//! this shell only relays. Absent either condition the assembler stays
//! unwired, so `rfc complete` skips the SEAL footer and `orkia rfc seal`
//! reports "not wired" (fail-closed).

use std::sync::Arc;

use orkia_capabilities::{Capability, CapabilityResolver};
use orkia_seal_proxy::KernelSealProxy;
use orkia_shell_types::RfcSealAssembler;

/// Build the kernel-backed SEAL assembler, or `None` when the capability
/// is locked or no daemon is reachable.
pub(crate) fn build(resolver: &Arc<dyn CapabilityResolver>) -> Option<Arc<dyn RfcSealAssembler>> {
    if !resolver.current().has(Capability::SealAuditExtended) {
        return None;
    }
    let kernel = orkia_kernel_client::discover()?;
    Some(Arc::new(KernelSealProxy::new(kernel)))
}

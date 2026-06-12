// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Shell-side implementation of the kernel's `ReasoningAudit` seam. The
//! cold-path sync worker lives in
//! `orkia-kernel`, which cannot depend on `orkia-shell` (circular), so it
//! holds a `dyn ReasoningAudit` and the shell provides the concrete sink.
//!
//! Each consolidated batch becomes one `reasoning.nodes_consolidated`
//! workspace-level event pushed through the shared `EventRouter`. The SEAL
//! consumer recognises the `reasoning.` prefix and appends it to the
//! workspace chain, making the chain the authoritative provenance log for
//! cloud-added knowledge (the node ids are enumerated in the sealed payload).
//!
//! CLAUDE.md #2 (one owner): this sends a one-way message onto the unified
//! `OrkiaEvent` channel — it never touches the SEAL store or its chain hash.

use orkia_kernel::ReasoningAudit;
use uuid::Uuid;

use crate::protocol::EventRouter;
use orkia_shell_types::JobId;

/// Routes cloud-consolidation audit batches into the SEAL workspace chain via
/// the unified event channel. Cheap to clone (the router holds `Arc`s).
pub struct EventRouterAudit {
    router: EventRouter,
}

impl EventRouterAudit {
    pub fn new(router: EventRouter) -> Self {
        Self { router }
    }
}

impl ReasoningAudit for EventRouterAudit {
    fn nodes_consolidated(&self, node_ids: &[Uuid], rfc_id: Option<&str>) {
        if node_ids.is_empty() {
            return;
        }
        let ids: Vec<String> = node_ids.iter().map(Uuid::to_string).collect();
        let data = serde_json::json!({
            "node_ids": ids,
            "count": node_ids.len(),
            "origin": "cloud",
        });
        let rfc = rfc_id.map(orkia_rfc_core::RfcId::new);
        // Job 0 = no originating job; the event is workspace-scoped. The SEAL
        // consumer routes `reasoning.*` onto the workspace chain regardless.
        self.router
            .on_custom_with_rfc(JobId(0), "", "reasoning.nodes_consolidated", data, rfc);
    }
}

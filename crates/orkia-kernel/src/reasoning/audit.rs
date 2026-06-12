// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! The reasoning → SEAL audit seam.
//!
//! The sync worker must record, on the workspace SEAL chain, every batch of
//! knowledge the cloud consolidated back into the local graph. But the worker
//! owns its store connection and the SEAL chain has a different owner (the seal
//! consumer); they must not share a handle (CLAUDE.md #2). So the worker holds a
//! `dyn ReasoningAudit` — a one-way *message* sink — and the shell implements it
//! over its `EventRouter`, which feeds the seal consumer. orkia-kernel cannot
//! depend on orkia-shell, so the trait lives here and the impl lives there
//! (dependency inversion).

use uuid::Uuid;

/// A sink for reasoning audit events, sealed on the workspace chain by the
/// shell. Implementations must be cheap and non-blocking (they run on the sync
/// worker's task) — a channel send, never I/O.
pub trait ReasoningAudit: Send + Sync {
    /// Record that the cloud consolidated `node_ids` into the workspace graph
    /// (emitted once per pull that wrote at least one node). `rfc_id` is the
    /// session-level RFC attribution, when any.
    fn nodes_consolidated(&self, node_ids: &[Uuid], rfc_id: Option<&str>);
}

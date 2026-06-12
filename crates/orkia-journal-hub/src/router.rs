// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Hook-router abstraction.
//!
//! The listener fires a router on every parsed `Hook` envelope so the
//! unified protocol pipeline (SEAL, metrics, …) sees events with zero
//! REPL-main-loop latency. The concrete router (`EventRouter`) lives in
//! `orkia-shell`; the hub depends only on this trait so it can be hosted
//! in either the REPL or the pty-daemon without a dependency edge back
//! into the shell.

use orkia_shell_types::journal::JournalEnvelope;

/// Routes a parsed journal envelope into the downstream event pipeline.
/// Implemented by `orkia-shell`'s `EventRouter`.
pub trait HookRouter: Send + Sync {
    /// Feed one envelope through the router. The return value mirrors the
    /// concrete router's `on_hook` (`true` when the envelope was a
    /// recognised hook that produced a downstream event); callers on the
    /// listener path ignore it.
    fn route_hook(&self, env: &JournalEnvelope) -> bool;
}

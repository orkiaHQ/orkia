// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! The capability *type* is now the single, shared
//! [`orkia_shell_types::CapabilitySet`] — the same type `CommandCtx` carries
//! for native commands. Here it is applied to a plugin **structurally**: the
//! granted set decides which host imports the wasmtime linker provides. With no
//! grant (total sandbox), the linker is empty, so a guest importing any effect
//! (`fetch`, `fs`, …) fails to instantiate — fail-closed by construction.
//!
//! For V1 the granted import set is **always empty**: effects flow through MCP
//! never through plugin host functions. When per-capability host
//! functions land, they are added here keyed on the granted `CapabilitySet`
//! scopes — never a blanket grant.

use orkia_shell_types::CapabilitySet;
use wasmtime::{Engine, Linker};

use crate::runtime::StoreState;

/// Build a linker exposing exactly the host functions the `caps` grant allows.
/// V1 grants no effect imports regardless of `caps` (effects route via MCP), so
/// this is always an empty linker — a guest importing any effect fails to
/// instantiate. The `caps` argument is the seam where future per-capability
/// host functions slot in, gated on the granted scopes.
pub fn linker(caps: &CapabilitySet, engine: &Engine) -> Linker<StoreState> {
    let _ = caps; // V1: no effect imports are ever defined (effects via MCP).
    Linker::new(engine)
}

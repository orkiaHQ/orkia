// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! The structured value model (`Value`).
//!
//! Moved to the lean, wasm-buildable `orkia-value` crate so the plugin SDK
//! (`orkia-plugin-sdk`, `wasm32-wasip1`) can share the *exact same* `Value` —
//! `orkia-shell-types` itself isn't wasm-buildable (native deps via
//! `orkia-terminal-core`). Re-exported here so every existing
//! `orkia_shell_types::exec::value::Value` / `orkia_shell_types::Value` path is

pub use orkia_value::Value;

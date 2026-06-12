// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! A plugin is a pipe transformer: it receives a `PipelineData`, computes, and
//! returns a `PipelineData`. Plugins are authored in TypeScript, compiled to a
//! QuickJS-WASM module, and executed under wasmtime — sandboxed by default
//! (no FS, no network, no clock), with effects routed through MCP, never the
//! plugin.
//!
//! This crate is the **public runtime** (loads + runs already-compiled
//! modules). The TS→WASM **compiler** lives in `orkia-plugin-build`
//! (feature-gated, pulled on demand) — not in the default binary.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod bridge;
pub mod command;
pub mod error;
pub mod gate;
pub mod manifest;
pub mod runtime;

pub use bridge::{json_to_value, value_to_json};
pub use command::PluginCommand;
pub use error::PluginError;
pub use manifest::{PluginManifest, parse_type};
pub use runtime::{LoadedPlugin, PluginRuntime, WasiRun};
// The unified effect-capability type is part of this crate's public API
// (`PluginCommand::new`, `run_*`). Re-export so consumers needn't depend on
pub use orkia_shell_types::CapabilitySet;

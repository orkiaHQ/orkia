// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use thiserror::Error;

/// Errors raised while loading, governing, or running a plugin.
#[derive(Debug, Error)]
pub enum PluginError {
    /// A `plugin.toml` (or inferred manifest) is malformed or references an
    /// unknown type.
    #[error("plugin manifest: {0}")]
    Manifest(String),

    /// A failure crossing the `Value ↔ JS` boundary.
    #[error("value bridge: {0}")]
    Bridge(String),

    /// The module could not be loaded (bad signature, undeserializable
    /// `.cwasm`, missing export). Fail-closed.
    #[error("plugin load: {0}")]
    Load(String),

    /// A failure during guest execution.
    #[error("plugin `{plugin}`: {message}")]
    Runtime { plugin: String, message: String },

    /// The guest tried to use a capability it was not granted — by
    /// construction the import is absent from the linker.
    #[error("capability denied for `{plugin}`: {message}")]
    CapabilityDenied { plugin: String, message: String },

    /// The guest exceeded a resource limit (fuel / memory) and was stopped by
    /// wasmtime without crashing the host.
    #[error("plugin `{plugin}` exceeded resource limit: {message}")]
    ResourceLimit { plugin: String, message: String },
}

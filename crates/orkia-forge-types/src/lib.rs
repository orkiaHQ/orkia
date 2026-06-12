// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Shared types for Orkia Forge: the on-disk manifest, the bridge protocol,
//! and the small error surface used by both the viewer and the builders.
//!
//! Nothing here knows about Tauri, the shell, or the journal — this crate is
//! the contract that lets `orkia-builtin`, `orkia-app-builtin`, and
//! `orkia-forge-viewer` agree on shapes without depending on each other.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod bridge;
pub mod manifest;
pub mod naming;

pub use bridge::{
    AgentResult, AgentStatus, BridgeError, BridgeMessage, BridgeResponse, FetchResponse,
    HttpMethod, NotifIcon,
};
pub use manifest::{
    ForgeConfig, ForgeManifest, ManifestError, Permissions, WindowConfig, default_api_version,
};
pub use naming::{NAME_PATTERN, validate_app_name};

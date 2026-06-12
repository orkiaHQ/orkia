// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Build mechanics for the Orkia Forge.
//!
//! This crate contains the local, HTTP-free logic invoked after a
//! successful Forge build call: writing artifacts to disk, hashing
//! RFCs for content addressing, and merging the local frontmatter
//! with the server-supplied manifest overrides.
//!
//! The HTTP client itself lives in the proprietary cloud client. This crate
//! is the deterministic, network-free half of the build pipeline.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod hash;
pub mod manifest;
pub mod orchestration;
pub mod response;
pub mod write;

pub use hash::{render_rfc_for_wire, sha256_hex};
pub use manifest::{build_manifest, load_previous_hash};
pub use orchestration::{BuildFromPathOpts, build_from_path, default_app_root, scaffold_dir_for};
pub use response::{BuildResponse, GeneratedFiles, ServerUsage};
pub use write::write_build;

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! V0 Forge builder: turns an RFC with `kind = "forge-app"` into a scaffold
//! at `~/.orkia/forge/<name>/`. V1 swaps this for a `RemoteBuilder` that
//! calls a remote builder service; the `ForgeBuilder` trait lives in
//! `orkia-shell-types` so the shell can hold a `Box<dyn ForgeBuilder>`.

mod placeholders;
mod scaffold;
mod validate;

pub use scaffold::{ScaffoldBuilder, build_from_path, default_app_root, scaffold_dir_for};
pub use validate::ValidatedForge;

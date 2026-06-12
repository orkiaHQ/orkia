// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! End-to-end orchestration of a Forge build.
//!
//! Composes the local build mechanics ([`crate::hash`], [`crate::write`],
//! [`crate::manifest`]) with a `&dyn ForgeBuilder` for the actual
//! network call. Public so any consumer — OSS shell, proprietary
//! shell, future surface mode — can drive a Forge build with the same
//! orchestration code, varying only the trait impl injected.

use std::path::{Path, PathBuf};

use orkia_forge_types::validate_app_name;
use orkia_rfc_core::{RfcId, RfcStore};
use orkia_shell_types::{BuildOutcome, BuilderError, ForgeBuilder};

use crate::{load_previous_hash, render_rfc_for_wire, sha256_hex};

/// User-facing flags for [`build_from_path`]. The HTTP-side
/// concerns (base URL, bearer) are now properties of the injected
/// [`ForgeBuilder`] and no longer appear in this struct.
#[derive(Debug, Clone)]
pub struct BuildFromPathOpts {
    /// Wipe the entire app dir (including `data/` + `seal/`) before
    /// rebuilding. Mutually exclusive with `rerun` semantically.
    pub force: bool,
    /// Preserve `data/` + `seal/`, allow rebuilding an existing app.
    /// When the current RFC hash matches the previous build's,
    /// returns [`BuilderError::RfcUnchanged`] *unless* `confirmed=true`.
    pub rerun: bool,
    /// Bypasses the unchanged-RFC check on `rerun`.
    pub confirmed: bool,
}

/// `~/.orkia/forge` — the root for every app's directory.
pub fn default_app_root() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".orkia").join("forge")
}

pub fn scaffold_dir_for(root: &Path, app_name: &str) -> PathBuf {
    root.join(app_name)
}

/// Higher-level entry that the `rfc forge` CLI uses with any
/// [`ForgeBuilder`] impl. Orchestrates the local pre-flight (RFC
/// load, dir resolution, rerun guard, hash comparison) and delegates
/// the actual build (HTTP + disk write) to the injected `forge`.
pub async fn build_from_path(
    project_dir: &Path,
    rfc_id: &str,
    root: &Path,
    opts: BuildFromPathOpts,
    forge: &dyn ForgeBuilder,
) -> Result<(PathBuf, BuildOutcome), BuilderError> {
    let BuildFromPathOpts {
        force,
        rerun,
        confirmed,
    } = opts;
    let store = RfcStore::new(project_dir);
    let rfc_id_typed = RfcId::new(rfc_id);
    let rfc = store
        .load(&rfc_id_typed)
        .map_err(|e| BuilderError::InvalidRfc(format!("RFC `{rfc_id}` not found: {e}")))?;

    // Validate BEFORE any filesystem operation (SEC-030): reject traversal
    // names (`../foo`, `/etc/...`) before joining or removing a directory.
    let validated_name = rfc
        .fm
        .forge
        .as_ref()
        .map(|f| f.name.clone())
        .ok_or_else(|| BuilderError::InvalidRfc("[forge] block missing".into()))?;
    validate_app_name(&validated_name)
        .map_err(|e| BuilderError::InvalidRfc(format!("forge.name invalid: {e}")))?;

    let app_dir = scaffold_dir_for(root, &validated_name);
    if app_dir.exists() {
        if !force && !rerun {
            return Err(BuilderError::AppExists {
                name: validated_name,
            });
        }
        if force {
            std::fs::remove_dir_all(&app_dir).map_err(BuilderError::Io)?;
        } else if rerun && !confirmed {
            let current_hash = sha256_hex(render_rfc_for_wire(&rfc).as_bytes());
            if let Some(prev) = load_previous_hash(&app_dir)
                && prev == current_hash
            {
                return Err(BuilderError::RfcUnchanged);
            }
        }
    }

    let outcome = forge.build(&rfc, &app_dir).await?;
    Ok((app_dir, outcome))
}

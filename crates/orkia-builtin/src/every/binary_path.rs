// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Resolve an absolute path to the `orkia` executable for the crontab
//! line. crond runs with a minimal PATH (usually just `/usr/bin:/bin`),
//! so a bare `orkia` would silently fail at fire time. We resolve once
//! at write time and bake the full path into the crontab entry.
//!
//!   1. `which orkia` — picks up shims and user PATH ordering.
//!   2. `~/.orkia/bin/orkia` — the install-script's standard location.
//!   3. `std::env::current_exe()` — last-ditch fallback so callers
//!      running from a `cargo run` build don't get a confusing error.
//!
//! Error message: "Could not resolve orkia binary path. Ensure orkia
//! is installed."

use std::path::PathBuf;
use std::process::Command;

pub fn resolve_orkia_binary() -> Result<PathBuf, String> {
    if let Some(p) = which_orkia() {
        return Ok(p);
    }
    if let Some(p) = home_install_path()
        && p.exists()
    {
        return Ok(p);
    }
    if let Ok(p) = std::env::current_exe()
        && p.exists()
    {
        return Ok(p);
    }
    Err("Could not resolve orkia binary path. Ensure orkia is installed.".into())
}

fn which_orkia() -> Option<PathBuf> {
    let out = Command::new("which").arg("orkia").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        return None;
    }
    let p = PathBuf::from(path);
    p.exists().then_some(p)
}

fn home_install_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".orkia/bin/orkia"))
}

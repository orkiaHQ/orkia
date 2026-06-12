// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Atomic config file helpers shared by the trust providers.
//!
//! Trust configs are the user's own agent settings (e.g. a 300 KB
//! `~/.claude.json`). We never truncate-in-place: write a temp file in
//! the same directory and rename over the target, so a crash mid-write
//! can't corrupt the original (CLAUDE.md: durability over speed).

use std::io::Write;
use std::path::Path;

use super::TrustError;

/// Read a file to a string, returning `None` when absent/unreadable.
pub(super) fn read_to_string(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Atomically write `bytes` to `path`: a temp file in the same parent
/// directory, fsync-free flush, then `rename(2)` over the target.
pub(super) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), TrustError> {
    let dir = path
        .parent()
        .ok_or_else(|| TrustError::Setup(format!("no parent dir for {}", path.display())))?;
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.flush()?;
    tmp.persist(path).map_err(|e| TrustError::Io(e.error))?;
    Ok(())
}

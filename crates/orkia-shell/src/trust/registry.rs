// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Orkia's own trust registry — the source of truth for "the user has
//! approved this directory". Persisted as a JSON array of absolute
//! paths. Orkia asks once per directory; on Yes the directory is added
//! here (and projected onto the provider via `pretrust`), so subsequent
//! dispatches in the same directory skip the consent prompt.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::TrustError;
use super::io::{read_to_string, write_atomic};

pub struct TrustRegistry {
    path: PathBuf,
    dirs: BTreeSet<String>,
}

impl TrustRegistry {
    /// Load the registry from `path` (e.g. `<data_dir>/trusted_dirs.json`).
    /// A missing or unreadable file yields an empty registry.
    pub fn load(path: PathBuf) -> Self {
        let dirs = read_to_string(&path)
            .and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
            .map(|v| v.into_iter().collect())
            .unwrap_or_default();
        Self { path, dirs }
    }

    pub fn is_trusted(&self, dir: &Path) -> bool {
        // Fail-closed on non-UTF-8 paths: a path that cannot be represented
        // as a valid UTF-8 string is never considered trusted, rather than
        // matching via a lossy U+FFFD replacement that could collide with a
        // different path.
        match dir.to_str() {
            Some(s) => self.dirs.contains(s),
            None => false,
        }
    }

    /// Record `dir` as trusted and persist. Idempotent.
    /// Returns `TrustError::NonUtf8Path` when `dir` contains non-UTF-8
    /// bytes — such paths cannot be stored without loss of information.
    pub fn trust(&mut self, dir: &Path) -> Result<(), TrustError> {
        let key = dir
            .to_str()
            .ok_or_else(|| TrustError::NonUtf8Path(dir.to_path_buf()))?
            .to_owned();
        if !self.dirs.insert(key) {
            return Ok(());
        }
        let list: Vec<&String> = self.dirs.iter().collect();
        let bytes =
            serde_json::to_vec_pretty(&list).map_err(|e| TrustError::Serialize(e.to_string()))?;
        write_atomic(&self.path, &bytes)
    }

    pub fn len(&self) -> usize {
        self.dirs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dirs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_persists_and_reloads() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("trusted_dirs.json");
        let dir = Path::new("/work/repo");

        let mut reg = TrustRegistry::load(path.clone());
        assert!(!reg.is_trusted(dir));
        reg.trust(dir).unwrap();
        assert!(reg.is_trusted(dir));

        // A fresh load sees the persisted entry.
        let reg2 = TrustRegistry::load(path);
        assert!(reg2.is_trusted(dir));
        assert_eq!(reg2.len(), 1);
    }

    #[test]
    fn trust_is_idempotent() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let mut reg = TrustRegistry::load(tmp.path().join("t.json"));
        let dir = Path::new("/a");
        reg.trust(dir).unwrap();
        reg.trust(dir).unwrap();
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn missing_file_is_empty() {
        let reg = TrustRegistry::load(PathBuf::from("/no/such/file.json"));
        assert!(reg.is_empty());
        assert!(!reg.is_trusted(Path::new("/x")));
    }
}

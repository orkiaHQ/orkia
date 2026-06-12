// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! One-shot migration from the legacy global `seal.jsonl` to the
//! scoped layout.
//!
//! Behaviour: if `<data_dir>/seal.jsonl` exists at startup, we
//! rename it to `<data_dir>/seal.jsonl.migrated`. Scoped chains
//! always start fresh — we don't try to demux the old monolith
//! does not attempt that). The renamed file remains queryable by
//! ad-hoc tooling.
//!
//! Idempotent: a second run sees no `seal.jsonl` and is a no-op.
//! If both `seal.jsonl` and `seal.jsonl.migrated` exist (e.g.
//! someone restored the old file manually), the existing
//! `.migrated` is preserved and we append a timestamp suffix to
//! the new one — never destructive.

use std::path::Path;

pub const LEGACY_FILE: &str = "seal.jsonl";
pub const MIGRATED_FILE: &str = "seal.jsonl.migrated";

/// Run the migration. Returns the new path of the moved file if a
/// rename occurred, or `None` if nothing was there to migrate.
pub fn migrate_global_seal(data_dir: &Path) -> Option<std::path::PathBuf> {
    let legacy = data_dir.join(LEGACY_FILE);
    if !legacy.exists() {
        return None;
    }
    let primary = data_dir.join(MIGRATED_FILE);
    let destination = if primary.exists() {
        // Keep the existing .migrated; suffix the new one so we
        // never overwrite history. Format: seal.jsonl.migrated-<rfc3339>.
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string();
        data_dir.join(format!("{MIGRATED_FILE}-{stamp}"))
    } else {
        primary
    };
    match std::fs::rename(&legacy, &destination) {
        Ok(()) => {
            tracing::info!(
                from = %legacy.display(),
                to = %destination.display(),
                "seal: migrated legacy global chain to scoped layout",
            );
            Some(destination)
        }
        Err(e) => {
            tracing::warn!(
                from = %legacy.display(),
                error = %e,
                "seal: failed to migrate legacy chain (will retry next start)",
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn renames_legacy_when_present() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(LEGACY_FILE), b"line1\nline2\n").unwrap();
        let moved = migrate_global_seal(dir.path()).expect("migrated");
        assert_eq!(moved, dir.path().join(MIGRATED_FILE));
        assert!(!dir.path().join(LEGACY_FILE).exists());
        assert_eq!(std::fs::read_to_string(&moved).unwrap(), "line1\nline2\n");
    }

    #[test]
    fn noop_when_no_legacy() {
        let dir = tempdir().unwrap();
        assert!(migrate_global_seal(dir.path()).is_none());
    }

    #[test]
    fn idempotent_across_runs() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(LEGACY_FILE), b"a\n").unwrap();
        migrate_global_seal(dir.path()).unwrap();
        // Second invocation: nothing to do.
        assert!(migrate_global_seal(dir.path()).is_none());
    }

    #[test]
    fn second_legacy_does_not_clobber_existing_migrated() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(MIGRATED_FILE), b"old-history\n").unwrap();
        std::fs::write(dir.path().join(LEGACY_FILE), b"new-history\n").unwrap();
        let moved = migrate_global_seal(dir.path()).expect("migrated");
        // Original .migrated is untouched; new file has a stamped name.
        assert_eq!(
            std::fs::read_to_string(dir.path().join(MIGRATED_FILE)).unwrap(),
            "old-history\n"
        );
        assert_ne!(moved, dir.path().join(MIGRATED_FILE));
        assert!(
            moved
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap()
                .starts_with(&format!("{MIGRATED_FILE}-"))
        );
    }
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Hermetic per-test environment.
//!
//! Owns a `tempfile::TempDir` that becomes the `HOME` for the
//! spawned `orkia` process. Lays out the directory shape orkia
//! expects (`.orkia/`, `.orkia/run/`, `.orkia/agents/`, `.orkia/seal/`)
//! so neither the shell nor the bridge has to create those at runtime
//! and race with the harness.
//!
//! The tempdir is RAII-cleaned when the sandbox is dropped, unless
//! `ORKIA_TEST_KEEP=1` is set — useful for post-mortem inspection.

use std::path::{Path, PathBuf};

use tempfile::TempDir;

/// Hermetic `ORKIA_HOME` for one test.
pub struct OrkiaSandbox {
    /// `None` only while we're about to leak the tempdir for keep-mode.
    dir: Option<TempDir>,
    /// Cached absolute path so we keep returning the same value after
    /// the optional `into_keep()` consumes the tempdir.
    home: PathBuf,
}

impl OrkiaSandbox {
    /// Create a fresh sandbox in the system tempdir.
    pub fn new() -> anyhow::Result<Self> {
        let dir = tempfile::Builder::new().prefix("orkia-e2e-").tempdir()?;
        let home = dir.path().to_path_buf();
        let me = Self {
            dir: Some(dir),
            home,
        };
        me.scaffold()?;
        Ok(me)
    }

    fn scaffold(&self) -> anyhow::Result<()> {
        for sub in [
            ".orkia",
            ".orkia/run",
            ".orkia/agents",
            ".orkia/seal",
            ".orkia/projects",
            ".orkia/cache",
        ] {
            std::fs::create_dir_all(self.home.join(sub))?;
        }
        // The store opens `<data_dir>/journal.jsonl` in append mode and
        // a missing file is treated as empty — we still touch it so the
        // harness can start its tail immediately without a file-watcher
        // setup race.
        let journal = self.home.join(".orkia/journal.jsonl");
        if !journal.exists() {
            std::fs::File::create(&journal)?;
        }
        // Pre-trust the sandbox cwd so `@agent` dispatch spawns directly
        // — mirrors a user's normal already-trusted working directory.
        // The trust gate itself is exercised by a test that first calls
        // [`Self::distrust`]. Canonicalised to match orkia's `agent_cwd`.
        let trusted = std::fs::canonicalize(&self.home).unwrap_or_else(|_| self.home.clone());
        let json = serde_json::to_vec(&[trusted.to_string_lossy().as_ref()])?;
        std::fs::write(self.home.join(".orkia/trusted_dirs.json"), json)?;
        Ok(())
    }

    /// Remove the pre-trust so the next `@agent` dispatch hits the trust
    /// consent gate. Used by the trust-flow test.
    pub fn distrust(&self) -> anyhow::Result<()> {
        let path = self.home.join(".orkia/trusted_dirs.json");
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Absolute path that will be passed as `HOME` to every subprocess.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// `<home>/.orkia/` — the orkia data directory.
    pub fn data_dir(&self) -> PathBuf {
        self.home.join(".orkia")
    }

    /// `<home>/.orkia/run/orkia.sock` — the journal Unix socket.
    pub fn socket_path(&self) -> PathBuf {
        self.home.join(".orkia/run/orkia.sock")
    }

    /// `<home>/.orkia/journal.jsonl` — the persisted journal NDJSON.
    pub fn journal_path(&self) -> PathBuf {
        self.home.join(".orkia/journal.jsonl")
    }

    /// `<home>/.orkia/agents/<name>` — agent definition directory.
    pub fn agent_dir(&self, name: &str) -> PathBuf {
        self.home.join(".orkia/agents").join(name)
    }

    /// `<home>/.orkia/seal/` — SEAL chain root.
    pub fn seal_dir(&self) -> PathBuf {
        self.home.join(".orkia/seal")
    }

    /// Consume the sandbox and leak the tempdir so the on-disk state
    /// survives the test (returns the kept path). Useful for debugging.
    pub fn into_keep(mut self) -> PathBuf {
        if let Some(dir) = self.dir.take() {
            let path = dir.keep();
            self.home = path.clone();
            path
        } else {
            self.home.clone()
        }
    }
}

impl Drop for OrkiaSandbox {
    fn drop(&mut self) {
        if std::env::var_os("ORKIA_TEST_KEEP").is_some()
            && let Some(dir) = self.dir.take()
        {
            let kept = dir.keep();
            eprintln!("[orkia-test-harness] kept sandbox: {}", kept.display());
        }
    }
}

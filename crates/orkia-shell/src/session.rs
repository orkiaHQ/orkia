// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! `~/.orkia/session.json` stores the bits that survive REPL restarts
//! and aren't already in `config.toml`. Today this is just
//! `current_team`; future work may add `current_project`,
//! `last_workspace`, etc.
//!
//! Loading is lenient: missing file → defaults, malformed JSON →
//! defaults + a warning trace. Old session files (without
//! `current_team`) deserialize cleanly thanks to `serde(default)`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const SESSION_FILENAME: &str = "session.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    /// UUID of the team the user has scoped operations to. `None`
    /// means "workspace-wide context".
    #[serde(default)]
    pub current_team: Option<Uuid>,
}

impl Session {
    /// Load from `<data_dir>/session.json`. Returns the default
    /// (empty) session on any read or parse error so the REPL never
    /// blocks startup on a corrupt session file.
    pub fn load(data_dir: &Path) -> Self {
        let path = Self::path(data_dir);
        if !path.exists() {
            return Self::default();
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "session: read failed; using defaults");
                return Self::default();
            }
        };
        match serde_json::from_str::<Session>(&text) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "session: parse failed; using defaults");
                Self::default()
            }
        }
    }

    /// Persist atomically (write tmp + rename) so a crash mid-write
    /// can't leave a half-truncated file behind.
    pub fn save(&self, data_dir: &Path) -> Result<(), std::io::Error> {
        std::fs::create_dir_all(data_dir)?;
        let path = Self::path(data_dir);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::other(format!("session serialize: {e}")))?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(SESSION_FILENAME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn missing_file_yields_default() {
        let dir = TempDir::new().unwrap();
        let s = Session::load(dir.path());
        assert!(s.current_team.is_none());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = TempDir::new().unwrap();
        let team = Uuid::new_v4();
        let s = Session {
            current_team: Some(team),
        };
        s.save(dir.path()).unwrap();
        let loaded = Session::load(dir.path());
        assert_eq!(loaded.current_team, Some(team));
    }

    #[test]
    fn old_session_without_current_team_loads_clean() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(SESSION_FILENAME), "{}").unwrap();
        let loaded = Session::load(dir.path());
        assert!(loaded.current_team.is_none());
    }

    #[test]
    fn malformed_json_falls_back_to_default() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(SESSION_FILENAME), "not json").unwrap();
        let loaded = Session::load(dir.path());
        assert!(loaded.current_team.is_none());
    }
}

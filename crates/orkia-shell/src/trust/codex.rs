// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Codex trust config: `~/.codex/config.toml`, with one table per
//! trusted directory:
//!
//! ```toml
//! [projects."/abs/dir"]
//! trust_level = "trusted"
//! ```
//!
//! We edit with `toml_edit` (format-preserving) so the user's existing
//! settings, comments, and key order survive — only the `trust_level`
//! for `dir` is added/updated.

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table, value};

use super::io::{read_to_string, write_atomic};
use super::{PreTrust, TrustError, TrustProvider};

pub struct CodexTrust {
    home: PathBuf,
}

impl CodexTrust {
    pub fn new(home: PathBuf) -> Self {
        Self { home }
    }

    fn config_path(&self) -> PathBuf {
        self.home.join(".codex").join("config.toml")
    }
}

impl TrustProvider for CodexTrust {
    fn name(&self) -> &str {
        "codex"
    }

    fn is_trusted(&self, dir: &Path) -> bool {
        // Fail-closed: a non-UTF-8 path cannot be a TOML table key.
        let Some(key) = dir.to_str() else {
            return false;
        };
        let Some(text) = read_to_string(&self.config_path()) else {
            return false;
        };
        let Ok(doc) = text.parse::<DocumentMut>() else {
            return false;
        };
        doc.get("projects")
            .and_then(|p| p.as_table_like())
            .and_then(|t| t.get(key))
            .and_then(|proj| proj.as_table_like())
            .and_then(|pt| pt.get("trust_level"))
            .and_then(|tl| tl.as_str())
            == Some("trusted")
    }

    fn pretrust(&self, dir: &Path) -> Result<PreTrust, TrustError> {
        // Fail-closed: reject non-UTF-8 paths — they cannot be stored as
        // TOML table keys without loss of information.
        let key = dir
            .to_str()
            .ok_or_else(|| TrustError::NonUtf8Path(dir.to_path_buf()))?
            .to_owned();
        let path = self.config_path();
        let text = read_to_string(&path).unwrap_or_default();
        let mut doc = text
            .parse::<DocumentMut>()
            .map_err(|e| TrustError::Parse(e.to_string()))?;

        // Build explicit STANDARD tables (not inline) so the result reads
        // `[projects."<dir>"]\ntrust_level = "trusted"` — codex's own
        // convention. `set_implicit` keeps the bare `[projects]` header
        // out (only the leaf table prints).
        let projects = doc
            .entry("projects")
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| TrustError::Parse("`projects` is not a table".into()))?;
        projects.set_implicit(true);
        let proj = projects
            .entry(&key)
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| TrustError::Parse("project entry is not a table".into()))?;
        proj["trust_level"] = value("trusted");

        write_atomic(&path, doc.to_string().as_bytes())?;
        Ok(PreTrust::Ensured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with(config: Option<&str>) -> tempfile::TempDir {
        let home = tempfile::TempDir::new().expect("tempdir");
        if let Some(c) = config {
            let dir = home.path().join(".codex");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("config.toml"), c).unwrap();
        }
        home
    }

    #[test]
    fn pretrust_then_is_trusted_no_prior_config() {
        let home = home_with(None);
        let p = CodexTrust::new(home.path().to_path_buf());
        let dir = Path::new("/work/repo");
        assert!(!p.is_trusted(dir));
        assert_eq!(p.pretrust(dir).unwrap(), PreTrust::Ensured);
        assert!(p.is_trusted(dir));
    }

    #[test]
    fn pretrust_preserves_existing_settings_and_comments() {
        let home = home_with(Some(
            "# my codex config\nmodel = \"o3\"\n\n[projects.\"/other\"]\ntrust_level = \"trusted\"\n",
        ));
        let p = CodexTrust::new(home.path().to_path_buf());
        p.pretrust(Path::new("/work/repo")).unwrap();

        let out = std::fs::read_to_string(home.path().join(".codex").join("config.toml")).unwrap();
        assert!(out.contains("# my codex config"), "comment preserved");
        assert!(out.contains("model = \"o3\""), "setting preserved");
        assert!(out.contains("/other"), "other project preserved");
        // New project trusted.
        assert!(p.is_trusted(Path::new("/work/repo")));
    }

    #[test]
    fn dir_with_slashes_renders_as_quoted_key() {
        let home = home_with(None);
        let p = CodexTrust::new(home.path().to_path_buf());
        p.pretrust(Path::new("/a/b/c")).unwrap();
        let out = std::fs::read_to_string(home.path().join(".codex").join("config.toml")).unwrap();
        assert!(out.contains("[projects.\"/a/b/c\"]"), "got: {out}");
        assert!(out.contains("trust_level = \"trusted\""));
    }
}

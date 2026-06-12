// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Claude trust config: `~/.claude.json`, a single JSON object with a
//! `projects` map keyed by absolute directory path; each entry carries
//! `hasTrustDialogAccepted` (and `hasCompletedProjectOnboarding`). We
//! read-modify-write the whole document with `serde_json::Value` so
//! every other key (caches, per-project metrics, 150+ projects) is
//! preserved verbatim — only the two trust flags for `dir` are set.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::io::{read_to_string, write_atomic};
use super::{PreTrust, TrustError, TrustProvider};

pub struct ClaudeTrust {
    home: PathBuf,
}

impl ClaudeTrust {
    pub fn new(home: PathBuf) -> Self {
        Self { home }
    }

    fn config_path(&self) -> PathBuf {
        self.home.join(".claude.json")
    }
}

impl TrustProvider for ClaudeTrust {
    fn name(&self) -> &str {
        "claude"
    }

    fn is_trusted(&self, dir: &Path) -> bool {
        // Fail-closed: a non-UTF-8 path cannot be a key in the JSON config.
        let Some(key) = dir.to_str() else {
            return false;
        };
        let Some(text) = read_to_string(&self.config_path()) else {
            return false;
        };
        let Ok(root) = serde_json::from_str::<Value>(&text) else {
            return false;
        };
        root.get("projects")
            .and_then(|p| p.get(key))
            .and_then(|e| e.get("hasTrustDialogAccepted"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    fn pretrust(&self, dir: &Path) -> Result<PreTrust, TrustError> {
        // Fail-closed: reject non-UTF-8 paths — they cannot be represented
        // as JSON object keys without loss of information.
        let key = dir
            .to_str()
            .ok_or_else(|| TrustError::NonUtf8Path(dir.to_path_buf()))?
            .to_owned();
        let path = self.config_path();
        let mut root = match read_to_string(&path) {
            Some(text) => serde_json::from_str::<Value>(&text)
                .map_err(|e| TrustError::Parse(e.to_string()))?,
            None => Value::Object(Map::new()),
        };
        let obj = root
            .as_object_mut()
            .ok_or_else(|| TrustError::Parse("claude config root is not an object".into()))?;
        let projects = obj
            .entry("projects")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .ok_or_else(|| TrustError::Parse("`projects` is not an object".into()))?;
        let entry = projects
            .entry(key)
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .ok_or_else(|| TrustError::Parse("project entry is not an object".into()))?;
        entry.insert("hasTrustDialogAccepted".into(), Value::Bool(true));
        entry.insert("hasCompletedProjectOnboarding".into(), Value::Bool(true));

        let bytes = serde_json::to_vec(&root).map_err(|e| TrustError::Parse(e.to_string()))?;
        write_atomic(&path, &bytes)?;
        Ok(PreTrust::Ensured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("tempdir")
    }

    #[test]
    fn pretrust_then_is_trusted_creates_config() {
        let home = tmp_home();
        let p = ClaudeTrust::new(home.path().to_path_buf());
        let dir = Path::new("/work/repo");
        assert!(!p.is_trusted(dir), "fresh: not trusted");
        assert_eq!(p.pretrust(dir).unwrap(), PreTrust::Ensured);
        assert!(p.is_trusted(dir), "after pretrust: trusted");
    }

    #[test]
    fn pretrust_preserves_existing_keys_and_other_projects() {
        let home = tmp_home();
        let path = home.path().join(".claude.json");
        std::fs::write(
            &path,
            r#"{"topLevelCache":42,"projects":{"/other":{"hasTrustDialogAccepted":true,"lastCost":1.5}}}"#,
        )
        .unwrap();
        let p = ClaudeTrust::new(home.path().to_path_buf());
        p.pretrust(Path::new("/work/repo")).unwrap();

        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // Untouched siblings survive.
        assert_eq!(v["topLevelCache"], 42);
        assert_eq!(v["projects"]["/other"]["lastCost"], 1.5);
        assert_eq!(v["projects"]["/other"]["hasTrustDialogAccepted"], true);
        // New project is trusted.
        assert_eq!(v["projects"]["/work/repo"]["hasTrustDialogAccepted"], true);
        assert_eq!(
            v["projects"]["/work/repo"]["hasCompletedProjectOnboarding"],
            true
        );
    }

    #[test]
    fn malformed_config_is_not_trusted_and_pretrust_errors() {
        let home = tmp_home();
        std::fs::write(home.path().join(".claude.json"), "{not json").unwrap();
        let p = ClaudeTrust::new(home.path().to_path_buf());
        assert!(!p.is_trusted(Path::new("/x")));
        assert!(p.pretrust(Path::new("/x")).is_err());
    }
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use orkia_forge_types::{ForgeManifest, ManifestError};

/// V0 discovery helper: an app on disk is a directory under
/// `~/.orkia/forge/<name>/` containing at minimum a `manifest.toml`.
///
/// We do not track which apps are currently running here — the REPL owns
/// `JobController` and joins `ForgeApp` with `JobInfo` at render time.
#[derive(Debug, Clone)]
pub struct ForgeApp {
    pub name: String,
    pub dir: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: ForgeManifest,
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoverError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("manifest: {0}")]
    Manifest(#[from] ManifestError),
    #[error("app `{0}` not found in {1}")]
    NotFound(String, PathBuf),
}

pub fn default_app_root() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".orkia").join("forge")
}

pub fn discover_all(root: &Path) -> Result<Vec<ForgeApp>, DiscoverError> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir = entry.path();
        let manifest_path = dir.join("manifest.toml");
        if !manifest_path.exists() {
            // Tolerate stray directories — they may be a half-aborted scaffold.
            continue;
        }
        let manifest = ForgeManifest::load(&manifest_path)?;
        let name = manifest.forge.name.clone();
        out.push(ForgeApp {
            name,
            dir,
            manifest_path,
            manifest,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub fn load_app(name: &str, root: &Path) -> Result<ForgeApp, DiscoverError> {
    let dir = root.join(name);
    let manifest_path = dir.join("manifest.toml");
    if !manifest_path.exists() {
        return Err(DiscoverError::NotFound(
            name.to_string(),
            root.to_path_buf(),
        ));
    }
    let manifest = ForgeManifest::load(&manifest_path)?;
    Ok(ForgeApp {
        name: manifest.forge.name.clone(),
        dir,
        manifest_path,
        manifest,
    })
}

/// Count lines in `<dir>/seal/events.jsonl`. Returns 0 if absent or unreadable
/// — SEAL events are a soft signal, not a load-bearing invariant.
pub fn seal_event_count(app: &ForgeApp) -> u64 {
    let p = app.dir.join("seal").join("events.jsonl");
    let Ok(s) = fs::read_to_string(&p) else {
        return 0;
    };
    s.lines().filter(|l| !l.trim().is_empty()).count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use orkia_forge_types::{ForgeConfig, Permissions, WindowConfig};
    use tempfile::TempDir;

    fn write_app(root: &Path, name: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let m = ForgeManifest {
            forge: ForgeConfig {
                name: name.into(),
                description: String::new(),
                version: "0.1.0".into(),
                api_version: 1,
                rfc_id: name.into(),
                rfc_hash: "sha256:0".into(),
                created_at: Utc::now(),
                icon: "default".into(),
                window: WindowConfig {
                    title: "T".into(),
                    width: 480,
                    height: 320,
                    resizable: true,
                },
                permissions: Permissions::default(),
            },
        };
        m.save(&dir.join("manifest.toml")).unwrap();
    }

    #[test]
    fn empty_root_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let apps = discover_all(tmp.path()).unwrap();
        assert!(apps.is_empty());
    }

    #[test]
    fn missing_root_returns_empty() {
        let apps = discover_all(Path::new("/nonexistent/forge/root")).unwrap();
        assert!(apps.is_empty());
    }

    #[test]
    fn discovers_and_sorts_by_name() {
        let tmp = TempDir::new().unwrap();
        write_app(tmp.path(), "zebra");
        write_app(tmp.path(), "alpha");
        write_app(tmp.path(), "mango");
        // Stray dir without manifest — must be skipped silently.
        fs::create_dir_all(tmp.path().join("scratch")).unwrap();
        let apps = discover_all(tmp.path()).unwrap();
        assert_eq!(apps.len(), 3);
        assert_eq!(apps[0].name, "alpha");
        assert_eq!(apps[1].name, "mango");
        assert_eq!(apps[2].name, "zebra");
    }

    #[test]
    fn load_app_finds_known() {
        let tmp = TempDir::new().unwrap();
        write_app(tmp.path(), "hello");
        let app = load_app("hello", tmp.path()).unwrap();
        assert_eq!(app.name, "hello");
    }

    #[test]
    fn load_app_missing_errors() {
        let tmp = TempDir::new().unwrap();
        assert!(matches!(
            load_app("nope", tmp.path()),
            Err(DiscoverError::NotFound(_, _))
        ));
    }

    #[test]
    fn seal_count_counts_nonblank_lines() {
        let tmp = TempDir::new().unwrap();
        write_app(tmp.path(), "hello");
        let seal_dir = tmp.path().join("hello").join("seal");
        fs::create_dir_all(&seal_dir).unwrap();
        fs::write(seal_dir.join("events.jsonl"), "a\nb\n\nc\n").unwrap();
        let app = load_app("hello", tmp.path()).unwrap();
        assert_eq!(seal_event_count(&app), 3);
    }
}

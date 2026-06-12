// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Curated archetype registry — clones/pulls `orkiaHQ/archetypes` into
//! `~/.orkia/registry/archetypes/` and exposes the available archetypes
//! as `ArchetypeMeta` so the wizard can render them in a menu.
//!
//! When the registry can't be reached (`--offline`, no network, no
//! `git` on PATH) the wizard falls back to the builtin set defined in
//! `super::builtins`.

use std::io;
use std::path::{Path, PathBuf};

const REGISTRY_REMOTE: &str = "https://github.com/orkiaHQ/archetypes.git";

#[derive(Debug, Clone)]
pub enum ArchetypeSource {
    /// On-disk archetype dir (must contain `archetype.toml` + optional
    /// `system-prompt.md`).
    Registry(PathBuf),
    /// Compiled-in default — `agent_templates::generate_prompt_template`
    /// renders the prompt at scaffold time.
    Builtin,
}

#[derive(Debug, Clone)]
pub struct ArchetypeMeta {
    pub name: String,
    pub description: String,
    pub suggested_names: Vec<String>,
    pub preferred_cli: Vec<String>,
    pub is_community: bool,
    pub source: ArchetypeSource,
}

#[derive(Debug, serde::Deserialize)]
struct ArchetypeFile {
    archetype: ArchetypeSection,
    #[serde(default)]
    defaults: DefaultsSection,
    #[serde(default)]
    compatibility: CompatibilitySection,
}

#[derive(Debug, serde::Deserialize)]
struct ArchetypeSection {
    name: String,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Default, serde::Deserialize)]
struct DefaultsSection {
    #[serde(default)]
    suggested_names: Vec<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct CompatibilitySection {
    #[serde(default)]
    preferred: Vec<String>,
}

pub struct ArchetypeRegistry {
    /// `<orkia_dir>/registry/archetypes/`
    path: PathBuf,
}

impl ArchetypeRegistry {
    pub fn new(orkia_dir: &Path) -> Self {
        Self {
            path: orkia_dir.join("registry").join("archetypes"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// True when the registry has already been cloned at least once.
    pub fn is_cached(&self) -> bool {
        self.path.join(".git").is_dir()
    }

    /// Clone or pull `orkiaHQ/archetypes`. Failures are logged and
    /// returned as `Err(message)`; the caller decides whether to fall
    /// back to a stale cache or to builtins.
    pub fn sync(&self) -> Result<(), String> {
        if self.is_cached() {
            run_git(&["-C", &self.path.to_string_lossy(), "pull", "--quiet"])
                .map_err(|e| format!("git pull: {e}"))
        } else {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("mkdir registry: {e}"))?;
            }
            run_git(&[
                "clone",
                "--depth",
                "1",
                "--quiet",
                REGISTRY_REMOTE,
                &self.path.to_string_lossy(),
            ])
            .map_err(|e| format!("git clone: {e}"))
        }
    }

    /// Walk the registry and return every parseable archetype. Returns
    /// an empty vec when the cache is missing — callers blend with
    /// builtins as appropriate.
    pub fn list(&self) -> Vec<ArchetypeMeta> {
        let mut out = Vec::new();
        collect_dir(&self.path, false, &mut out);
        let community = self.path.join("community");
        if community.is_dir() {
            collect_dir(&community, true, &mut out);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}

fn collect_dir(root: &Path, is_community: bool, out: &mut Vec<ArchetypeMeta>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if name_str.starts_with('.') || name_str == "community" {
            continue;
        }
        if let Some(meta) = load_archetype(&path, is_community) {
            out.push(meta);
        }
    }
}

fn load_archetype(dir: &Path, is_community: bool) -> Option<ArchetypeMeta> {
    let toml_path = dir.join("archetype.toml");
    let body = std::fs::read_to_string(&toml_path).ok()?;
    let parsed: ArchetypeFile = match toml::from_str(&body) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(path = %toml_path.display(), error = %e, "invalid archetype.toml");
            return None;
        }
    };
    Some(ArchetypeMeta {
        name: parsed.archetype.name,
        description: parsed.archetype.description,
        suggested_names: parsed.defaults.suggested_names,
        preferred_cli: parsed.compatibility.preferred,
        is_community,
        source: ArchetypeSource::Registry(dir.to_path_buf()),
    })
}

fn run_git(args: &[&str]) -> io::Result<()> {
    let status = std::process::Command::new("git").args(args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("git exited with status {status}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_archetype(dir: &Path, name: &str, desc: &str, preferred: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        let pref_arr = preferred
            .iter()
            .map(|p| format!("\"{p}\""))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            dir.join("archetype.toml"),
            format!(
                "[archetype]\nname = \"{name}\"\ndescription = \"{desc}\"\n\n\
                 [defaults]\nsuggested_names = [\"faye\"]\n\n\
                 [compatibility]\npreferred = [{pref_arr}]\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn list_returns_official_and_community_sorted() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join(".orkia");
        std::fs::create_dir_all(root.join("registry").join("archetypes")).unwrap();
        let reg = ArchetypeRegistry::new(&root);
        write_archetype(
            &reg.path.join("software-eng"),
            "software-eng",
            "engineer",
            &["claude"],
        );
        write_archetype(
            &reg.path.join("community").join("rust-specialist"),
            "rust-specialist",
            "rust",
            &["claude", "codex"],
        );

        let list = reg.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "rust-specialist");
        assert!(list[0].is_community);
        assert_eq!(list[1].name, "software-eng");
        assert!(!list[1].is_community);
    }

    #[test]
    fn is_cached_false_when_no_git_dir() {
        let tmp = TempDir::new().unwrap();
        let reg = ArchetypeRegistry::new(tmp.path());
        assert!(!reg.is_cached());
    }
}

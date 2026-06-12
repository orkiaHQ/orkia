// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Filesystem-backed project/rfc/issue store.
//!
//! Layout under `~/.orkia/projects/`:
//!
//! ```text
//! projects/
//!   <project-slug>/
//!     project.toml
//!     rfcs/<slug>.md         (TOML frontmatter delimited by `+++`)
//!     issues/<NNN>-<slug>.toml
//! ```

mod frontmatter;
mod io;
mod types;
mod util;

pub use frontmatter::parse_rfc_frontmatter;
pub use types::{IssueSummary, Project, RfcFrontmatter, RfcSummary, Workspace};
pub use util::slug;

use std::path::{Path, PathBuf};

use crate::scope::Scope;

use frontmatter::{
    read_frontmatter_field, rewrite_frontmatter_array, rewrite_frontmatter_scalar,
    split_frontmatter,
};
use io::{find_issue_path, next_issue_number, scan_projects};
use util::{toml_escape, upsert_toml_field};

// ─── Loading ────────────────────────────────────────────────────────────────

impl Workspace {
    /// Scan `<data_dir>/projects/` and load every project.
    pub fn load(data_dir: &Path) -> Self {
        let root = data_dir.join("projects");
        let projects = if root.exists() {
            scan_projects(&root)
        } else {
            Vec::new()
        };
        Self { projects, root }
    }

    /// Reload from disk. Call after any mutation (create/update).
    pub fn reload(&mut self) {
        self.projects = if self.root.exists() {
            scan_projects(&self.root)
        } else {
            Vec::new()
        };
    }

    pub fn project(&self, name: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.name == name)
    }

    /// Create a new project directory with `project.toml`, `rfcs/`, `issues/`.
    ///
    /// PR1b: `scope` is always `None` from current callers but the
    /// writer knows how to emit it; PR2 adds the user-facing `--scope`
    /// flag and threads a real value through here.
    pub fn create_project(
        root: &Path,
        name: &str,
        description: Option<&str>,
        scope: Option<Scope>,
    ) -> std::io::Result<PathBuf> {
        let dir = root.join(slug(name));
        std::fs::create_dir_all(dir.join("rfcs"))?;
        std::fs::create_dir_all(dir.join("issues"))?;
        let desc = description.unwrap_or("");
        let scope_line = match scope {
            Some(s) => format!("scope = \"{}\"\n", s.as_str()),
            None => String::new(),
        };
        let toml = format!(
            "[project]\nname = \"{}\"\ndescription = \"{}\"\ncreated_at = \"{}\"\n{}\n[agents]\nassigned = []\n",
            toml_escape(name),
            toml_escape(desc),
            chrono::Utc::now().to_rfc3339(),
            scope_line,
        );
        std::fs::write(dir.join("project.toml"), toml)?;
        Ok(dir)
    }

    /// Create an rfc markdown file with default frontmatter.
    /// Create a new RFC for the given project. Delegates to
    /// [`orkia_rfc_core::RfcStore::create_with_legacy`] so the frontmatter
    /// carries both the state-machine fields (`id`, `state`, `version`,
    /// `content_hash`) and the legacy mirrors (`title`, `status`,
    /// `assigned`) the workspace loader uses for the RFC list view.
    pub fn create_rfc(
        project_path: &Path,
        title: &str,
        assigned: &[String],
    ) -> std::io::Result<PathBuf> {
        let s = slug(title);
        let store = orkia_rfc_core::RfcStore::new(project_path.to_path_buf());
        let id = orkia_rfc_core::RfcId::new(&s);
        store
            .create_with_legacy(&id, Some(title), assigned)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        // Append the legacy section skeleton so authors get a usable
        // template. The state machine only cares about the frontmatter;
        // body content is free-form.
        let path = project_path.join("rfcs").join(format!("{s}.md"));
        let existing = std::fs::read_to_string(&path)?;
        let with_skeleton = format!(
            "{existing}\n# {title}\n\n## Objective\n\n\n## Constraints\n\n\n## Acceptance Criteria\n\n",
        );
        std::fs::write(&path, with_skeleton)?;
        Ok(path)
    }

    /// Create a TOML issue file with an auto-incremented number.
    ///
    /// PR1b: same contract as `create_project` — `scope` is accepted
    /// and serialized, but every current caller passes `None`. PR2
    /// wires the `--scope` flag.
    pub fn create_issue(
        project_path: &Path,
        title: &str,
        priority: &str,
        scope: Option<Scope>,
    ) -> std::io::Result<PathBuf> {
        use std::io::Write as _;

        let issues_dir = project_path.join("issues");
        std::fs::create_dir_all(&issues_dir)?;
        let s = slug(title);
        let scope_line = match scope {
            Some(s) => format!("scope = \"{}\"\n", s.as_str()),
            None => String::new(),
        };
        let content = format!(
            "[issue]\ntitle = \"{}\"\nstatus = \"todo\"\npriority = \"{}\"\nassigned = \"\"\ncreated_at = \"{}\"\ndescription = \"\"\n{}",
            toml_escape(title),
            toml_escape(priority),
            chrono::Utc::now().to_rfc3339(),
            scope_line,
        );

        // Atomically reserve a number with an `O_EXCL` temp file keyed on the
        // number alone (not the slug), then rename into place. Two concurrent
        // `create_issue` calls can no longer pick the same number — the loser of
        // the exclusive create simply increments and retries. A crash between
        // create and rename leaves a stale `.NNN.tmp` that only skips a number;
        // it never causes a collision or a deadlock (BUG-106).
        let mut n = next_issue_number(project_path);
        loop {
            let tmp = issues_dir.join(format!(".{n:03}.tmp"));
            match std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&tmp)
            {
                Ok(mut f) => {
                    f.write_all(content.as_bytes())?;
                    f.sync_all()?;
                    drop(f);
                    let path = issues_dir.join(format!("{n:03}-{s}.toml"));
                    std::fs::rename(&tmp, &path)?;
                    return Ok(path);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    n += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Resolve the active project name for an RFC command.
    ///
    /// Priority:
    /// 1. Explicit `--project` flag.
    /// 2. `default_project` from `config.toml` (must match a real project).
    /// 3. cwd ancestor whose `file_name()` matches a project name.
    pub fn resolve_project_name(
        &self,
        flag: Option<&str>,
        cwd: &Path,
        config_default: Option<&str>,
    ) -> Option<String> {
        if let Some(name) = flag {
            return Some(name.to_string());
        }
        if let Some(name) = config_default
            && self.projects.iter().any(|p| p.name == name)
        {
            return Some(name.to_string());
        }
        let names: Vec<&str> = self.projects.iter().map(|p| p.name.as_str()).collect();
        for ancestor in cwd.ancestors() {
            if let Some(component) = ancestor.file_name().and_then(|s| s.to_str())
                && let Some(hit) = names.iter().find(|n| **n == component)
            {
                return Some((*hit).to_string());
            }
        }
        None
    }
}

// ─── RFC / Issue / Project store operations ─────────────────────────────────

impl Workspace {
    /// Update a field on an RFC frontmatter. Supports scalar and array fields.
    ///
    /// Scalar fields: `status`, `title`, `priority`.
    /// Array fields: `assigned`, `tags` — `value` is parsed as comma-separated.
    ///
    /// Returns `(path, old_value)`; arrays are serialized as comma-separated for audit.
    pub fn update_rfc(
        project_path: &Path,
        slug: &str,
        field: &str,
        value: &str,
    ) -> std::io::Result<(PathBuf, String)> {
        let path = project_path.join("rfcs").join(format!("{slug}.md"));
        let content = std::fs::read_to_string(&path)?;
        let (fm_str, body) = split_frontmatter(&content).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("rfc '{slug}' has no frontmatter"),
            )
        })?;

        let is_array = matches!(field, "assigned" | "tags");
        let old_value = read_frontmatter_field(fm_str, field).unwrap_or_default();
        let new_fm = if is_array {
            let items: Vec<String> = value
                .split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
            rewrite_frontmatter_array(fm_str, field, &items)
        } else {
            rewrite_frontmatter_scalar(fm_str, field, value)
        };

        let mut out = String::with_capacity(content.len());
        out.push_str("+++\n");
        out.push_str(new_fm.trim_matches('\n'));
        out.push_str("\n+++\n");
        out.push_str(body);
        std::fs::write(&path, out)?;
        Ok((path, old_value))
    }

    /// Update a field on an issue. Limited to known scalar fields.
    pub fn update_issue(
        project_path: &Path,
        number: u32,
        field: &str,
        value: &str,
    ) -> std::io::Result<PathBuf> {
        let path = find_issue_path(project_path, number).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("issue {number} not found"),
            )
        })?;
        let content = std::fs::read_to_string(&path)?;
        let updated = upsert_toml_field(&content, "[issue]", field, value);
        std::fs::write(&path, updated)?;
        Ok(path)
    }

    /// Update an existing project's `project.toml`. Accepts an optional
    /// new description and/or scope; pass `None` to leave a field
    /// unchanged. Both edits are upserts: if the field isn't already in
    /// the file, it is inserted under the `[project]` header.
    pub fn update_project(
        project_path: &Path,
        description: Option<&str>,
        scope: Option<Scope>,
    ) -> std::io::Result<()> {
        let path = project_path.join("project.toml");
        let mut content = std::fs::read_to_string(&path)?;
        if let Some(desc) = description {
            content = upsert_toml_field(&content, "[project]", "description", desc);
        }
        if let Some(scope) = scope {
            content = upsert_toml_field(&content, "[project]", "scope", scope.as_str());
        }
        std::fs::write(&path, content)
    }
}

// === PendingPromptQueue extension for the `attention list` builtin ===
// (this is in orkia-shell; we include a note here for grep-ability)

#[cfg(test)]
#[path = "tests.rs"]
mod tests_mod;

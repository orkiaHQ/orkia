// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Scope-override validation at write time.
//!
//! permissive than their parent, never more permissive. This module gives
//! the REPL a single entry point to enforce that rule before any TOML
//! mutation reaches disk.
//!
//! Parent resolution:
//! - For project create/update: parent = workspace `default_scope` (or Private).
//! - For RFC/issue create/update: parent = effective scope of the containing
//!   project (project scope falling back to workspace default).

use std::path::Path;

use orkia_shell_types::scope::{resolve_effective_scope, validate_override};
use orkia_shell_types::{Scope, Workspace};

use crate::config::read_default_scope;

/// Validate that `proposed` is a legal scope for an artifact whose containing
/// project (if any) is `project_name`. Returns `Err(message)` if the override
/// would be more permissive than the parent.
///
/// `project_name = None` signals a project-level artifact whose only parent
/// is the workspace default. `Some(name)` signals an RFC/issue whose parent
/// is the named project's effective scope.
pub fn validate_artifact_scope(
    data_dir: &Path,
    workspace: &Workspace,
    project_name: Option<&str>,
    proposed: Scope,
) -> Result<(), String> {
    let workspace_default = read_default_scope(data_dir);
    let project_scope = project_name
        .and_then(|n| workspace.project(n))
        .and_then(|p| p.scope);
    let parent = resolve_effective_scope(workspace_default, project_scope, None, None);
    validate_override(parent, proposed).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_default_scope(data_dir: &Path, scope: Scope) {
        std::fs::create_dir_all(data_dir).unwrap();
        std::fs::write(
            data_dir.join("config.toml"),
            format!("default_scope = \"{}\"\n", scope.as_str()),
        )
        .unwrap();
    }

    fn empty_workspace(data_dir: &Path) -> Workspace {
        std::fs::create_dir_all(data_dir.join("projects")).unwrap();
        Workspace::load(data_dir)
    }

    // ─── project-level (parent = workspace default) ──────────────────────

    #[test]
    fn project_legal_override_under_public_default() {
        let dir = tempdir().unwrap();
        write_default_scope(dir.path(), Scope::Public);
        let ws = empty_workspace(dir.path());
        assert!(validate_artifact_scope(dir.path(), &ws, None, Scope::Team).is_ok());
        assert!(validate_artifact_scope(dir.path(), &ws, None, Scope::Private).is_ok());
        assert!(validate_artifact_scope(dir.path(), &ws, None, Scope::Public).is_ok());
    }

    #[test]
    fn project_illegal_override_above_private_default() {
        let dir = tempdir().unwrap();
        // No config written → default_scope unset → parent = Private.
        let ws = empty_workspace(dir.path());
        let err = validate_artifact_scope(dir.path(), &ws, None, Scope::Public).unwrap_err();
        assert!(err.contains("illegal"), "got: {err}");
        let err = validate_artifact_scope(dir.path(), &ws, None, Scope::Team).unwrap_err();
        assert!(err.contains("illegal"), "got: {err}");
    }

    #[test]
    fn project_illegal_public_under_team_default() {
        let dir = tempdir().unwrap();
        write_default_scope(dir.path(), Scope::Team);
        let ws = empty_workspace(dir.path());
        let err = validate_artifact_scope(dir.path(), &ws, None, Scope::Public).unwrap_err();
        assert!(err.contains("illegal"), "got: {err}");
    }

    // ─── RFC/issue (parent = project's effective scope) ──────────────────

    #[test]
    fn child_legal_override_under_public_project() {
        let dir = tempdir().unwrap();
        let mut ws = empty_workspace(dir.path());
        Workspace::create_project(&ws.root, "alpha", None, Some(Scope::Public)).unwrap();
        ws.reload();
        assert!(validate_artifact_scope(dir.path(), &ws, Some("alpha"), Scope::Team).is_ok());
        assert!(validate_artifact_scope(dir.path(), &ws, Some("alpha"), Scope::Private).is_ok());
    }

    #[test]
    fn child_illegal_public_in_private_project() {
        let dir = tempdir().unwrap();
        let mut ws = empty_workspace(dir.path());
        Workspace::create_project(&ws.root, "secret", None, Some(Scope::Private)).unwrap();
        ws.reload();
        let err =
            validate_artifact_scope(dir.path(), &ws, Some("secret"), Scope::Public).unwrap_err();
        assert!(err.contains("illegal"), "got: {err}");
        let err =
            validate_artifact_scope(dir.path(), &ws, Some("secret"), Scope::Team).unwrap_err();
        assert!(err.contains("illegal"), "got: {err}");
    }

    #[test]
    fn child_inherits_workspace_default_when_project_unset() {
        let dir = tempdir().unwrap();
        write_default_scope(dir.path(), Scope::Team);
        let mut ws = empty_workspace(dir.path());
        // Project with no explicit scope → falls back to workspace default = Team.
        Workspace::create_project(&ws.root, "beta", None, None).unwrap();
        ws.reload();
        assert!(validate_artifact_scope(dir.path(), &ws, Some("beta"), Scope::Team).is_ok());
        assert!(validate_artifact_scope(dir.path(), &ws, Some("beta"), Scope::Private).is_ok());
        let err =
            validate_artifact_scope(dir.path(), &ws, Some("beta"), Scope::Public).unwrap_err();
        assert!(err.contains("illegal"), "got: {err}");
    }
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! RFC document CRUD + operator constraints — the `impl Repl` half of the `rfc`
//! builtin that touches RFC *content* (List / Show / Create / Edit / Update /
//! Delegate / Remove / Constraints). The state-machine ops live in
//! [`super::rfc_ops`], forge/seal in [`super::forge`], and the command router
//! plus project/slug resolution in [`super::rfc`].

use super::*;

fn quoted_csv(values: &[String]) -> String {
    values
        .iter()
        .map(|v| format!("\"{}\"", v.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ")
}

fn shell_csv(values: &[String]) -> String {
    if values.is_empty() {
        "\"\"".to_string()
    } else {
        values.join(",")
    }
}

impl Repl {
    /// RFC document CRUD: List / Show / Create / Edit / Update / Delegate / Remove.
    pub(super) async fn handle_rfc_doc(
        &mut self,
        action: orkia_builtin::rfc::RfcAction,
    ) -> Outcome {
        use orkia_builtin::rfc::{self, RfcAction};
        match action {
            RfcAction::List { project, status } => Outcome::BuiltinOutput {
                blocks: rfc::list(&self.workspace, project.as_deref(), status.as_deref()),
            },
            RfcAction::Show { slug, project } => self.handle_rfc_show(slug, project),
            RfcAction::Create {
                title,
                project,
                assigned,
                scope,
            } => {
                self.handle_rfc_create(title, project, assigned, scope)
                    .await
            }
            RfcAction::Edit { slug, project } => self.handle_rfc_edit(slug, project).await,
            RfcAction::Update {
                slug,
                project,
                field,
                value,
            } => self.handle_rfc_update(slug, project, field, value).await,
            RfcAction::Delegate {
                slug,
                project,
                agent,
            } => self.handle_rfc_doc_delegate(slug, project, agent).await,
            RfcAction::Remove {
                slug,
                project,
                force,
            } => self.handle_rfc_remove(slug, project, force).await,
            RfcAction::ConstraintsPropose { slug, project } => {
                self.handle_rfc_constraints_propose(slug, project)
            }
            RfcAction::ConstraintsAccept {
                slug,
                project,
                allowed_paths,
                forbidden_paths,
                forbidden_commands,
                risk_ceiling,
                watch_paths,
            } => self.handle_rfc_constraints_accept(
                slug,
                project,
                orkia_rfc_core::frontmatter::OperatorConstraints {
                    allowed_paths,
                    forbidden_paths,
                    forbidden_commands,
                    risk_ceiling,
                    watch_paths,
                    // Declared authoritatively in RFC frontmatter, not via this
                    // accept path (see rfc_constraints.rs).
                    contract_paths: Vec::new(),
                },
            ),
            // Safety: inner is only called with the doc-family variants.
            _ => unreachable!("handle_rfc_doc: unexpected variant"),
        }
    }

    /// RFC show: resolve project (with slug fallback), render RFC content.
    fn handle_rfc_show(&self, slug: String, project: Option<String>) -> Outcome {
        use orkia_builtin::rfc;
        let project = match self.resolve_rfc_target(project.as_deref(), &slug) {
            Ok(p) => p,
            Err(o) => return o,
        };
        Outcome::BuiltinOutput {
            blocks: rfc::show(&self.workspace, &project, &slug),
        }
    }

    /// RFC edit: resolve project, locate file, open editor with seal tracking.
    async fn handle_rfc_edit(&mut self, slug: String, project: Option<String>) -> Outcome {
        use orkia_builtin::rfc;
        let project = match self.resolve_rfc_target(project.as_deref(), &slug) {
            Ok(p) => p,
            Err(o) => return o,
        };
        let path = match rfc::locate(&self.workspace, &project, &slug) {
            Ok(p) => p,
            Err(blocks) => return Outcome::BuiltinOutput { blocks },
        };
        self.spawn_editor_and_seal(&path, &slug, &project).await
    }

    /// RFC delegate (doc path): resolve project, then dispatch.
    async fn handle_rfc_doc_delegate(
        &mut self,
        slug: String,
        project: Option<String>,
        agent: String,
    ) -> Outcome {
        let project = match self.resolve_rfc_project(project.as_deref()) {
            Ok(p) => p,
            Err(o) => return o,
        };
        self.dispatch_rfc_delegate(&slug, &project, &agent).await
    }

    /// RFC create: validate scope, write file, emit event, open editor.
    async fn handle_rfc_create(
        &mut self,
        title: String,
        project: Option<String>,
        assigned: Vec<String>,
        scope: Option<orkia_shell_types::Scope>,
    ) -> Outcome {
        use orkia_builtin::rfc;
        let project = match self.resolve_rfc_project(project.as_deref()) {
            Ok(p) => p,
            Err(o) => return o,
        };
        if let Some(s) = scope
            && let Err(e) = orkia_builtin::scope_validation::validate_artifact_scope(
                &self.config.data_dir,
                &self.workspace,
                Some(&project),
                s,
            )
        {
            return Outcome::BuiltinOutput {
                blocks: vec![BlockContent::Error(e)],
            };
        }
        let path = match rfc::create(&self.workspace, &project, &title, &assigned, scope) {
            Ok(p) => p,
            Err(blocks) => return Outcome::BuiltinOutput { blocks },
        };
        let slug = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        self.emit_rfc_create_events(&slug, &project, &path, &title, scope);
        self.workspace.reload();
        self.emit_workspace_snapshot();
        self.spawn_editor_and_seal(&path, &slug, &project).await
    }

    /// Emit the create event and optional scope events after rfc::create succeeds.
    fn emit_rfc_create_events(
        &mut self,
        slug: &str,
        project: &str,
        path: &std::path::Path,
        title: &str,
        scope: Option<orkia_shell_types::Scope>,
    ) {
        let hash = crate::seal::rfc_content_hash(path);
        self.event_router.on_custom_with_rfc(
            JobId(0),
            "",
            "rfc.create",
            serde_json::json!({
                "slug": slug,
                "project": project,
                "title": title,
                "content_hash": hash,
            }),
            Some(orkia_rfc_core::RfcId::new(slug)),
        );
        let Some(scope) = scope else { return };
        let artifact_id = format!("{project}/{slug}");
        self.emit_scope_event_local("rfc.scope_set", Some(project), &artifact_id, None, scope);
        let warn = self.maybe_warn_team_scope(Some(scope), &artifact_id);
        for block in &warn {
            self.notification_queue.push(match block {
                BlockContent::SystemInfo(s) | BlockContent::Text(s) | BlockContent::Error(s) => {
                    s.clone()
                }
                _ => String::new(),
            });
        }
    }

    /// RFC update: validate scope change, write field, emit events.
    async fn handle_rfc_update(
        &mut self,
        slug: String,
        project: Option<String>,
        field: String,
        value: String,
    ) -> Outcome {
        use orkia_builtin::rfc;
        let project = match self.resolve_rfc_project(project.as_deref()) {
            Ok(p) => p,
            Err(o) => return o,
        };
        if field == "scope"
            && let Ok(proposed) = orkia_shell_types::Scope::parse(&value)
            && let Err(e) = orkia_builtin::scope_validation::validate_artifact_scope(
                &self.config.data_dir,
                &self.workspace,
                Some(&project),
                proposed,
            )
        {
            return Outcome::BuiltinOutput {
                blocks: vec![BlockContent::Error(e)],
            };
        }
        let (path, old) = match rfc::update(&self.workspace, &project, &slug, &field, &value) {
            Ok(r) => r,
            Err(blocks) => return Outcome::BuiltinOutput { blocks },
        };
        self.emit_rfc_update_events(&slug, &project, &field, &value, &old, &path);
        self.workspace.reload();
        self.emit_workspace_snapshot();
        Outcome::BuiltinOutput {
            blocks: vec![BlockContent::SystemInfo(format!(
                "rfc {slug}: {field} {old} → {value}"
            ))],
        }
    }

    /// Emit the update, complete, and scope-change events after rfc::update succeeds.
    fn emit_rfc_update_events(
        &mut self,
        slug: &str,
        project: &str,
        field: &str,
        value: &str,
        old: &str,
        path: &std::path::Path,
    ) {
        self.event_router.on_custom_with_rfc(
            JobId(0),
            "",
            "rfc.update",
            serde_json::json!({
                "slug": slug,
                "project": project,
                "field": field,
                "old": old,
                "new": value,
            }),
            Some(orkia_rfc_core::RfcId::new(slug)),
        );
        if field == "status" && value == "completed" {
            let hash = crate::seal::rfc_content_hash(path);
            self.event_router.on_custom_with_rfc(
                JobId(0),
                "",
                "rfc.complete",
                serde_json::json!({"slug": slug, "project": project, "content_hash": hash}),
                Some(orkia_rfc_core::RfcId::new(slug)),
            );
        }
        if field == "scope"
            && let Ok(new_scope) = orkia_shell_types::Scope::parse(value)
        {
            let previous = orkia_shell_types::Scope::parse(old).ok();
            if previous != Some(new_scope) {
                let artifact_id = format!("{project}/{slug}");
                self.emit_scope_event_local(
                    "rfc.scope_changed",
                    Some(project),
                    &artifact_id,
                    previous,
                    new_scope,
                );
            }
        }
    }

    /// RFC remove: safety check, delete file, emit event.
    async fn handle_rfc_remove(
        &mut self,
        slug: String,
        project: Option<String>,
        force: bool,
    ) -> Outcome {
        use orkia_builtin::rfc;
        let project = match self.resolve_rfc_project(project.as_deref()) {
            Ok(p) => p,
            Err(o) => return o,
        };
        if !force {
            return Outcome::BuiltinOutput {
                blocks: vec![BlockContent::SystemInfo(format!(
                    "rfc remove: re-run with --force to delete '{slug}' from {project}"
                ))],
            };
        }
        let path = match rfc::locate(&self.workspace, &project, &slug) {
            Ok(p) => p,
            Err(blocks) => return Outcome::BuiltinOutput { blocks },
        };
        if let Err(e) = std::fs::remove_file(&path) {
            return Outcome::Error(format!("failed to remove rfc: {e}"));
        }
        self.event_router.on_custom_with_rfc(
            JobId(0),
            "",
            "rfc.remove",
            serde_json::json!({ "slug": slug, "project": project }),
            Some(orkia_rfc_core::RfcId::new(slug.clone())),
        );
        self.workspace.reload();
        self.emit_workspace_snapshot();
        Outcome::BuiltinOutput {
            blocks: vec![BlockContent::SystemInfo(format!("rfc '{slug}' removed"))],
        }
    }

    fn handle_rfc_constraints_propose(&self, slug: String, project: Option<String>) -> Outcome {
        use orkia_builtin::rfc;
        let project = match self.resolve_rfc_target(project.as_deref(), &slug) {
            Ok(p) => p,
            Err(o) => return o,
        };
        let path = match rfc::locate(&self.workspace, &project, &slug) {
            Ok(p) => p,
            Err(blocks) => return Outcome::BuiltinOutput { blocks },
        };
        let project_root = path
            .parent()
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf);
        let Some(project_root) = project_root else {
            return Outcome::Error("rfc constraints: could not resolve project root".into());
        };
        let store = orkia_rfc_core::RfcStore::new(project_root.clone());
        let id = orkia_rfc_core::RfcId::new(slug.clone());
        let record = match store.load(&id) {
            Ok(record) => record,
            Err(e) => return Outcome::Error(e.to_string()),
        };
        let proposal =
            crate::rfc_constraints::propose(&self.config.data_dir, &project_root, &id, &record);
        let constraints = proposal.constraints;
        Outcome::BuiltinOutput {
            blocks: vec![
                BlockContent::SystemInfo(format!(
                    "proposed operator constraints for rfc {slug} (project {project})"
                )),
                BlockContent::Text(format!(
                    "allowed_paths = [{}]",
                    quoted_csv(&constraints.allowed_paths)
                )),
                BlockContent::Text(format!(
                    "forbidden_paths = [{}]",
                    quoted_csv(&constraints.forbidden_paths)
                )),
                BlockContent::Text(format!(
                    "forbidden_commands = [{}]",
                    quoted_csv(&constraints.forbidden_commands)
                )),
                BlockContent::Text(format!(
                    "risk_ceiling = \"{}\"",
                    constraints.risk_ceiling.as_deref().unwrap_or("high")
                )),
                BlockContent::Text(format!(
                    "watch_paths = [{}]",
                    quoted_csv(&constraints.watch_paths)
                )),
                BlockContent::Text(format!("sources = [{}]", quoted_csv(&proposal.sources))),
                BlockContent::SystemInfo(format!(
                    "accept with: orkia rfc constraints accept {slug} --allowed {} --forbidden {} --forbid-cmd {} --risk {} --watch {}",
                    shell_csv(&constraints.allowed_paths),
                    shell_csv(&constraints.forbidden_paths),
                    shell_csv(&constraints.forbidden_commands),
                    constraints.risk_ceiling.as_deref().unwrap_or("high"),
                    shell_csv(&constraints.watch_paths)
                )),
            ],
        }
    }

    fn handle_rfc_constraints_accept(
        &mut self,
        slug: String,
        project: Option<String>,
        constraints: orkia_rfc_core::frontmatter::OperatorConstraints,
    ) -> Outcome {
        use orkia_builtin::rfc;
        use orkia_rfc_core::frontmatter::OperatorFrontmatterBlock;

        let project = match self.resolve_rfc_target(project.as_deref(), &slug) {
            Ok(p) => p,
            Err(o) => return o,
        };
        let path = match rfc::locate(&self.workspace, &project, &slug) {
            Ok(p) => p,
            Err(blocks) => return Outcome::BuiltinOutput { blocks },
        };
        let project_root = path
            .parent()
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf);
        let Some(project_root) = project_root else {
            return Outcome::Error("rfc constraints: could not resolve project root".into());
        };
        let store = orkia_rfc_core::RfcStore::new(project_root);
        let id = orkia_rfc_core::RfcId::new(slug.clone());
        let mut rec = match store.load(&id) {
            Ok(rec) => rec,
            Err(e) => return Outcome::Error(e.to_string()),
        };
        rec.fm.operator = Some(OperatorFrontmatterBlock {
            constraints: Some(constraints.clone()),
        });
        if let Err(e) = store.save(rec.fm, rec.body) {
            return Outcome::Error(e.to_string());
        }
        self.event_router.on_custom_with_rfc(
            JobId(0),
            "",
            "rfc.constraints_set",
            serde_json::json!({
                "rfc_id": slug,
                "project": project,
                "allowed_paths": constraints.allowed_paths,
                "forbidden_paths": constraints.forbidden_paths,
                "forbidden_commands": constraints.forbidden_commands,
                "risk_ceiling": constraints.risk_ceiling,
                "watch_paths": constraints.watch_paths,
            }),
            Some(id),
        );
        self.workspace.reload();
        Outcome::BuiltinOutput {
            blocks: vec![BlockContent::SystemInfo(format!(
                "rfc {slug}: operator constraints accepted"
            ))],
        }
    }
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

/// Outcome of a workspace-wide slug lookup (see [`locate_rfc_slug`]).
pub(crate) enum RfcSlugMatch {
    /// Exactly one project owns the slug.
    Project(String),
    /// No project owns it.
    NotFound,
    /// Two or more projects own it — sorted project names for a stable message.
    Ambiguous(Vec<String>),
}

/// Find which project(s) own an RFC slug. Standalone (no `&self`) so the
/// resolution rule is unit-testable against a bare `Workspace`.
pub(crate) fn locate_rfc_slug(workspace: &Workspace, slug: &str) -> RfcSlugMatch {
    let mut hits: Vec<String> = workspace
        .projects
        .iter()
        .filter(|p| p.rfcs.iter().any(|r| r.slug == slug))
        .map(|p| p.name.clone())
        .collect();
    match hits.len() {
        0 => RfcSlugMatch::NotFound,
        1 => RfcSlugMatch::Project(hits.remove(0)),
        _ => {
            hits.sort();
            RfcSlugMatch::Ambiguous(hits)
        }
    }
}

/// Double-quote a token for the runtime's `tokenize_args` re-parse when it
/// contains whitespace (project names may; slugs/agent names normally don't).
fn quote_arg(s: &str) -> String {
    if s.chars().any(char::is_whitespace) {
        format!("\"{s}\"")
    } else {
        s.to_string()
    }
}

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
    /// Resolve the project for an rfc command. Precedence: explicit
    /// `--project` > active `rfc cd` scope > config default > cwd ancestor
    /// match. Returns an Outcome::Error if nothing resolves.
    ///
    /// The `rfc cd` scope is an explicit "I'm working inside this RFC's
    /// project" statement (T2.59), so it outranks the passive config/cwd
    /// defaults and feeds every subsequent op — the slug defaulting in
    /// [`Self::workspace_default_rfc_slug`] reads the same scope.
    pub(crate) fn resolve_rfc_project(&self, flag: Option<&str>) -> Result<String, Outcome> {
        if flag.is_none()
            && let Some(scope) = &self.rfc_scope
        {
            return Ok(scope.project.clone());
        }
        let cwd = std::env::current_dir().unwrap_or_default();
        self.workspace
            .resolve_project_name(flag, &cwd, self.config.default_project.as_deref())
            .ok_or_else(|| Outcome::Error("no project specified and no default available".into()))
    }

    /// Resolve the project for a *slug-addressed read* (`rfc show`).
    ///
    /// A slug is an identity: `rfc show auth-refresh` names the RFC, so when
    /// no project context resolves (flag / default / cwd / `rfc cd` scope) we
    /// fall back to a workspace-wide slug lookup — mirroring how `rfc list`
    /// works project-less. Unambiguous → resolve; collision → refuse and list
    /// the candidates (fail-closed, never guess); absent → not-found.
    pub(crate) fn resolve_rfc_target(
        &self,
        flag: Option<&str>,
        slug: &str,
    ) -> Result<String, Outcome> {
        if let Ok(project) = self.resolve_rfc_project(flag) {
            return Ok(project);
        }
        match locate_rfc_slug(&self.workspace, slug) {
            RfcSlugMatch::Project(project) => Ok(project),
            RfcSlugMatch::NotFound => Err(Outcome::Error(format!(
                "rfc '{slug}' not found in any project"
            ))),
            RfcSlugMatch::Ambiguous(projects) => Err(Outcome::Error(format!(
                "rfc '{slug}' exists in {}; disambiguate with --project",
                projects.join(", ")
            ))),
        }
    }

    pub(crate) async fn handle_rfc(&mut self, args: &[String]) -> Outcome {
        let outcome = self.handle_rfc_inner(args).await;
        // Any rfc-* builtin may have mutated frontmatter or added a
        // decision; refresh the cached scope segment so the next
        // prompt reflects current state without per-prompt disk I/O.
        if self.rfc_scope.is_some() {
            self.refresh_rfc_scope_segment();
        }
        outcome
    }

    pub(crate) async fn handle_rfc_inner(&mut self, args: &[String]) -> Outcome {
        use orkia_builtin::rfc::{self, RfcAction};
        match rfc::parse(args) {
            Ok(
                action @ (RfcAction::List { .. }
                | RfcAction::Show { .. }
                | RfcAction::Create { .. }
                | RfcAction::Edit { .. }
                | RfcAction::Update { .. }
                | RfcAction::Delegate { .. }
                | RfcAction::Remove { .. }
                | RfcAction::ConstraintsPropose { .. }
                | RfcAction::ConstraintsAccept { .. }),
            ) => self.handle_rfc_doc(action).await,
            Ok(
                action @ (RfcAction::State { .. }
                | RfcAction::Promote { .. }
                | RfcAction::Complete { .. }
                | RfcAction::Abandon { .. }
                | RfcAction::Reopen { .. }
                | RfcAction::LockStatus { .. }
                | RfcAction::ReleaseLock { .. }
                | RfcAction::Ask { .. }
                | RfcAction::Resolve { .. }
                | RfcAction::Cd { .. }),
            ) => self.handle_rfc_state_action(action).await,
            Ok(action) => self.handle_rfc_forge_seal(action).await,
            Err(e) => Outcome::Error(e),
        }
    }

    /// RFC document CRUD: List / Show / Create / Edit / Update / Delegate / Remove.
    async fn handle_rfc_doc(&mut self, action: orkia_builtin::rfc::RfcAction) -> Outcome {
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

    /// RFC state-machine operations: State / Promote / Complete / Abandon /
    /// Reopen / LockStatus / ReleaseLock / Ask / Resolve / Cd.
    ///
    /// Each resolves the active project, instantiates an RfcStateService rooted
    /// at the project directory, invokes the operation, and forwards the buffered
    /// RfcEvents through `event_router.on_custom` so the SEAL chain picks them up.
    async fn handle_rfc_state_action(&mut self, action: orkia_builtin::rfc::RfcAction) -> Outcome {
        use orkia_builtin::rfc::RfcAction;
        match action {
            RfcAction::State { slug, project } => self.handle_rfc_state(slug, project),
            RfcAction::Promote {
                slug,
                project,
                confirm,
            } => {
                self.handle_rfc_transition(slug, project, RfcTransitionOp::Promote, confirm)
                    .await
            }
            RfcAction::Complete {
                slug,
                project,
                confirm,
            } => {
                self.handle_rfc_transition(slug, project, RfcTransitionOp::Complete, confirm)
                    .await
            }
            RfcAction::Abandon {
                slug,
                project,
                reason,
                confirm,
            } => {
                self.handle_rfc_transition(slug, project, RfcTransitionOp::Abandon(reason), confirm)
                    .await
            }
            RfcAction::Reopen {
                slug,
                project,
                confirm,
            } => {
                self.handle_rfc_transition(slug, project, RfcTransitionOp::Reopen, confirm)
                    .await
            }
            RfcAction::LockStatus { slug, project } => self.handle_rfc_lock_status(slug, project),
            RfcAction::ReleaseLock { slug, project } => self.handle_rfc_release_lock(slug, project),
            RfcAction::Ask {
                slug,
                project,
                question,
                rationale,
            } => self.handle_rfc_ask(slug, project, question, rationale),
            RfcAction::Resolve {
                slug,
                project,
                decision_id,
                answer,
            } => self.handle_rfc_resolve(slug, project, decision_id, answer),
            RfcAction::Cd { slug, project } => self.handle_rfc_cd(slug, project),
            // Safety: inner is only called with the state-machine variants.
            _ => unreachable!("handle_rfc_state_action: unexpected variant"),
        }
    }

    pub(crate) fn handle_rfc_cd(&mut self, slug: String, project: Option<String>) -> Outcome {
        let project = match self.resolve_rfc_project(project.as_deref()) {
            Ok(p) => p,
            Err(o) => return o,
        };
        let Some(p) = self.workspace.project(&project) else {
            return Outcome::Error(format!("project '{project}' not found"));
        };
        // Confirm the RFC exists on disk so `cd` doesn't enter a
        // never-was scope. Anything more elaborate (state checks etc.)
        // is the prompt renderer's job.
        let store = orkia_rfc_core::RfcStore::new(p.path.clone());
        let id = orkia_rfc_core::RfcId::new(&slug);
        if store.load(&id).is_err() {
            return Outcome::Error(format!("rfc '{slug}' not found in {project}"));
        }
        self.rfc_scope = Some(RfcScopeState {
            project: project.clone(),
            rfc_id: id,
        });
        // Eagerly load the segment so the next prompt avoids any disk
        // I/O — invariant #1.
        self.refresh_rfc_scope_segment();
        Outcome::BuiltinOutput {
            blocks: vec![BlockContent::SystemInfo(format!(
                "rfc scope: {slug} (project {project})"
            ))],
        }
    }

    /// Spawn `$EDITOR` on an RFC; if the file content changes, emit `rfc.edit`.
    pub(crate) async fn spawn_editor_and_seal(
        &mut self,
        path: &Path,
        slug: &str,
        project: &str,
    ) -> Outcome {
        let old_hash = crate::seal::rfc_content_hash(path);
        let outcome = self.spawn_editor(path).await;
        let new_hash = crate::seal::rfc_content_hash(path);
        if old_hash != new_hash {
            self.emit_audit_event(
                JobId(0),
                "",
                "rfc.edit",
                serde_json::json!({
                    "slug": slug,
                    "project": project,
                    "old_hash": old_hash,
                    "new_hash": new_hash,
                }),
            );
        }
        outcome
    }

    pub(crate) async fn dispatch_rfc_delegate(
        &mut self,
        slug: &str,
        project: &str,
        agent: &str,
    ) -> Outcome {
        use orkia_builtin::rfc;
        let path = match rfc::locate(&self.workspace, project, slug) {
            Ok(p) => p,
            Err(blocks) => return Outcome::BuiltinOutput { blocks },
        };
        let hash = crate::seal::rfc_content_hash(&path);

        let (cmd, args) = match self.config.resolve_agent(agent) {
            Some((cmd, args)) => (cmd.to_string(), args.to_vec()),
            None => {
                return Outcome::AgentStarted {
                    agent: agent.into(),
                    job_id: self.config.agent_unresolved_reason(agent),
                };
            }
        };
        // delegation spawns in the `pty_daemon` so the session survives REPL
        // exit. Forward the canonical doc-command line — the runtime re-parses
        // it through the identical classifier → dispatch and re-runs this flow
        // in-process (its `detached_spawner` is None, the recursion guard).
        // `--project` pins the project the REPL just resolved, so the runtime
        // skips its own scope resolution. The `rfc.delegate` audit is NOT
        // emitted here: the runtime emits it right before its own `agent.spawn`,
        // preserving the delegate-before-spawn ordering inside the journal
        // stream that actually carries the spawn.
        if let Some(spawner) = self.detached_spawner.clone() {
            let line = format!(
                "orkia rfc delegate {} --agent {} --project {}",
                quote_arg(slug),
                quote_arg(agent),
                quote_arg(project),
            );
            let mut req = orkia_shell_types::DetachedSpawnRequest::new(line);
            req.working_dir = self.agent_cwd().map(|d| d.display().to_string());
            req.agent_name = Some(agent.to_string());
            return match spawner.spawn_detached(req) {
                Ok(daemon_id) => Outcome::JobSpawned {
                    job_id: JobId(daemon_id),
                    foreground: false,
                    owner: JobOwner::Daemon,
                },
                Err(e) => Outcome::Error(format!("failed to spawn agent via daemon: {e}")),
            };
        }

        let (agent_context, extra_env, hooks_provider) =
            self.build_agent_context(Some(agent)).await;
        // Emit BEFORE the spawn — the SEAL consumer correlates this with the
        // subsequent `agent.spawn` to attribute delegation to the right project
        // chain. Order matters.
        self.emit_rfc_delegate_audit(slug, project, agent, &hash);
        let input = AgentJobConfigInput {
            agent_name: agent,
            cmd: &cmd,
            args: &args,
            extra_env,
            agent_context,
            hooks_provider: hooks_provider.as_deref(),
            stdin: orkia_shell_types::StdinSource::Pty,
            pending_body: None,
            project: Some(project.to_string()),
            working_dir: self.agent_cwd(),
            cage_wrapper: self.cage_wrapper(agent),
        };
        self.spawn_rfc_agent_job(input).await
    }

    /// Emit the rfc.delegate audit event before the agent spawn.
    fn emit_rfc_delegate_audit(&mut self, slug: &str, project: &str, agent: &str, hash: &str) {
        self.emit_audit_event(
            JobId(0),
            "",
            "rfc.delegate",
            serde_json::json!({
                "slug": slug,
                "project": project,
                "agent": agent,
                "content_hash": hash,
            }),
        );
    }

    /// Spawn an agent job from a pre-built `AgentJobConfigInput` for RFC delegation.
    async fn spawn_rfc_agent_job(&mut self, input: AgentJobConfigInput<'_>) -> Outcome {
        let agent_name = input.agent_name;
        // Capture the RFC's project before `build_agent_job_config` consumes the
        // input, so the reasoning scope attributes turns to the right project.
        let project = input.project.clone();
        let config = build_agent_job_config(input);
        let deps = crate::job::spawn::SpawnDeps {
            approvals: &self.approvals,
            event_router: &self.event_router,
            state_machine: &self.state_machine,
            injection_executor: &self.injection_executor,
            job_projects: &self.job_projects,
            agent_name,
        };
        match self.jobs.spawn(config, deps) {
            Ok(result) => {
                self.record_reasoning_scope(result.job_id, project.as_deref())
                    .await;
                Outcome::JobSpawned {
                    job_id: result.job_id,
                    foreground: false,
                    owner: JobOwner::Local,
                }
            }
            Err(e) => Outcome::Error(format!("failed to spawn agent: {e}")),
        }
    }

    pub(crate) async fn spawn_editor(&mut self, path: &Path) -> Outcome {
        let editor = std::env::var("EDITOR")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "vi".to_string());
        // brush handles quoting; the editor inherits the orkia PTY slave.
        let cmd = format!("{editor} {}", shell_escape(&path.display().to_string()));
        let outcome = self.dispatch_shell(&cmd, false).await;
        self.workspace.reload();
        self.emit_workspace_snapshot();
        outcome
    }
}

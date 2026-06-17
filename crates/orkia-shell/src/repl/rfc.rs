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
            Ok(RfcAction::Dispatch {
                slug,
                project,
                resume,
            }) => self.handle_rfc_dispatch(slug, project, resume).await,
            Ok(RfcAction::DispatchTask {
                rfc_id,
                project,
                task,
                agent,
            }) => {
                self.handle_rfc_dispatch_task(rfc_id, project, task, agent)
                    .await
            }
            Ok(action) => self.handle_rfc_forge_seal(action).await,
            Err(e) => Outcome::Error(e),
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
            req.cage_wrapper = self.detached_cage(agent);
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
    pub(super) async fn spawn_rfc_agent_job(&mut self, input: AgentJobConfigInput<'_>) -> Outcome {
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

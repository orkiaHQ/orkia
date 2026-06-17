// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `orkia rfc dispatch` — RFC → many-agents fan-out (`SPEC-ORKIA-RFC-DISPATCH`,
//! step 6). The user-facing `dispatch` reads the RFC's `[dispatch]` block,
//! gates on Team membership, and hands a [`DispatchRequest`] to the OSS
//! [`KernelDispatchProxy`], which authorizes the DAG with the kernel and drives
//! the run on its own thread (the REPL is back at the prompt immediately, §1).
//!
//! `dispatch-task` is the per-task execution entry the proxy's detached jobs
//! re-parse: it reads the composed prompt back out of `<rfc_dir>/issues/<id>.md`
//! and spawns a real interactive agent with it as `pending_body` — never print
//! mode (§5). It carries no Team gate; the parent run was already authorized.

use super::*;

use orkia_dispatch_proxy::{
    DispatchRequest, DispatchStartOutcome, DispatchTaskSpec, ResumeOutcome, RunHandle,
};

impl Repl {
    /// Start (or `--resume`) a dispatch run for the named RFC. Team-gated:
    /// unset proxy or no team membership refuses before any task spawns.
    pub(crate) async fn handle_rfc_dispatch(
        &mut self,
        slug: String,
        project: Option<String>,
        resume: bool,
    ) -> Outcome {
        let Some(proxy) = self.dispatch_proxy.clone() else {
            return Outcome::Error(
                "RFC dispatch requires Orkia Team. See https://orkia.dev/team".into(),
            );
        };
        if let Some(refusal) = self.dispatch_team_gate().await {
            return refusal;
        }
        let req = match self.build_dispatch_request(&slug, project.as_deref()) {
            Ok(r) => r,
            Err(o) => return o,
        };
        // The authorize round-trip to the kernel is blocking I/O; keep it off
        // the REPL thread (§1). The actor it launches drives the run itself.
        let outcome = tokio::task::spawn_blocking(move || {
            if resume {
                DispatchOutcome::Resume(proxy.resume_run(req))
            } else {
                DispatchOutcome::Start(proxy.start_run(req))
            }
        })
        .await;
        match outcome {
            Ok(DispatchOutcome::Start(o)) => render_start(&slug, o, &mut self.dispatch_runs),
            Ok(DispatchOutcome::Resume(o)) => render_resume(&slug, o, &mut self.dispatch_runs),
            Err(e) => Outcome::Error(format!("rfc {slug}: dispatch task panicked: {e}")),
        }
    }

    /// Re-run one dispatch task in this (detached) runtime: read its composed
    /// prompt from the issues store and spawn an interactive agent with it.
    pub(crate) async fn handle_rfc_dispatch_task(
        &mut self,
        rfc_id: String,
        project: Option<String>,
        task: String,
        agent: String,
    ) -> Outcome {
        let project_name = match self.resolve_rfc_target(project.as_deref(), &rfc_id) {
            Ok(p) => p,
            Err(o) => return o,
        };
        let Some(p) = self.workspace.project(&project_name) else {
            return Outcome::Error(format!("project '{project_name}' not found"));
        };
        let store = orkia_rfc_core::RfcStore::new(p.path.clone());
        let id = orkia_rfc_core::RfcId::new(&rfc_id);
        let Some(rfc_dir) = store.rfc_path(&id).parent().map(Path::to_path_buf) else {
            return Outcome::Error("rfc path has no parent directory".into());
        };
        let prompt = match read_task_prompt(&rfc_dir, &task) {
            Ok(body) => body,
            Err(o) => return o,
        };
        self.spawn_dispatch_task_agent(&agent, &project_name, prompt)
            .await
    }

    /// Server-side Team-membership check, mirroring `dispatch_pipeline`. An
    /// `Unavailable` backend falls through (OSS build with the proxy wired by a
    /// richer distribution already made its allow decision). `Some` = refuse.
    async fn dispatch_team_gate(&self) -> Option<Outcome> {
        match self.team_client.me().await {
            Ok(me) if me.teams.is_empty() => Some(Outcome::Error(
                "RFC dispatch requires team membership. Join a team first.".into(),
            )),
            Ok(_) => None,
            Err(orkia_shell_types::TeamClientError::Unavailable { .. }) => None,
            Err(e) => Some(Outcome::Error(format!(
                "Could not verify team membership: {}",
                orkia_shell_types::team_error_message(&e)
            ))),
        }
    }

    /// Load the RFC, read its `[dispatch]` block, and build the proxy request.
    fn build_dispatch_request(
        &self,
        slug: &str,
        project: Option<&str>,
    ) -> Result<DispatchRequest, Outcome> {
        let project_name = self.resolve_rfc_target(project, slug)?;
        let Some(p) = self.workspace.project(&project_name) else {
            return Err(Outcome::Error(format!(
                "project '{project_name}' not found"
            )));
        };
        let store = orkia_rfc_core::RfcStore::new(p.path.clone());
        let id = orkia_rfc_core::RfcId::new(slug);
        let record = store
            .load(&id)
            .map_err(|e| Outcome::Error(format!("rfc '{slug}': {e}")))?;
        let Some(block) = record.fm.dispatch else {
            return Err(Outcome::Error(format!(
                "rfc '{slug}' has no [dispatch] block — add one before dispatching"
            )));
        };
        let rfc_dir = store
            .rfc_path(&id)
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| Outcome::Error("rfc path has no parent directory".into()))?;
        let tasks = block
            .tasks
            .into_iter()
            .map(|t| DispatchTaskSpec {
                id: t.id,
                agent: t.agent,
                body: t.body,
                depends_on: t.depends_on,
                accept: t.accept,
                max_attempts: t.max_attempts,
            })
            .collect();
        Ok(DispatchRequest {
            rfc_id: slug.to_string(),
            project: project_name,
            rfc_dir,
            working_dir: self.agent_cwd().map(|d| d.display().to_string()),
            strategy: block.strategy,
            max_inflight: block.max_inflight,
            on_task_fail: block.on_task_fail,
            tasks,
        })
    }

    /// Spawn one task's agent interactively with the composed prompt as
    /// `pending_body` (detector-gated injection). Sibling of the non-detached
    /// branch of `dispatch_rfc_delegate`; this never re-detaches (it already
    /// runs inside the proxy's detached job).
    async fn spawn_dispatch_task_agent(
        &mut self,
        agent: &str,
        project: &str,
        prompt: String,
    ) -> Outcome {
        let (cmd, args) = match self.config.resolve_agent(agent) {
            Some((cmd, args)) => (cmd.to_string(), args.to_vec()),
            None => {
                return Outcome::AgentStarted {
                    agent: agent.into(),
                    job_id: self.config.agent_unresolved_reason(agent),
                };
            }
        };
        let (agent_context, extra_env, hooks_provider) =
            self.build_agent_context(Some(agent)).await;
        let input = AgentJobConfigInput {
            agent_name: agent,
            cmd: &cmd,
            args: &args,
            extra_env,
            agent_context,
            hooks_provider: hooks_provider.as_deref(),
            stdin: orkia_shell_types::StdinSource::Pty,
            pending_body: Some(prompt),
            project: Some(project.to_string()),
            working_dir: self.agent_cwd(),
            cage_wrapper: self.cage_wrapper(agent),
        };
        self.spawn_rfc_agent_job(input).await
    }
}

/// Result carrier so the `spawn_blocking` closure returns one type.
enum DispatchOutcome {
    Start(DispatchStartOutcome),
    Resume(ResumeOutcome),
}

/// Read the composed prompt for `task` from `<rfc_dir>/issues/<task>.md`.
fn read_task_prompt(rfc_dir: &Path, task: &str) -> Result<String, Outcome> {
    let store = orkia_dispatch_proxy::issues::IssueStore::new(rfc_dir);
    match store.read(task) {
        Ok(Some(issue)) => Ok(issue.prompt),
        Ok(None) => Err(Outcome::Error(format!(
            "dispatch-task: no issue for task `{task}` under {}",
            rfc_dir.display()
        ))),
        Err(e) => Err(Outcome::Error(format!(
            "dispatch-task: issue `{task}` unreadable: {e}"
        ))),
    }
}

fn render_start(slug: &str, outcome: DispatchStartOutcome, runs: &mut Vec<RunHandle>) -> Outcome {
    match outcome {
        DispatchStartOutcome::Started {
            total_tasks,
            handle,
        } => {
            let run_id = handle.run_id().to_string();
            runs.push(handle);
            Outcome::BuiltinOutput {
                blocks: vec![BlockContent::SystemInfo(format!(
                    "rfc {slug}: dispatch run {run_id} started — {total_tasks} task(s). Track with `ps` / `wait`."
                ))],
            }
        }
        DispatchStartOutcome::Refused { reason } => {
            Outcome::Error(format!("rfc {slug}: dispatch refused: {reason}"))
        }
    }
}

fn render_resume(slug: &str, outcome: ResumeOutcome, runs: &mut Vec<RunHandle>) -> Outcome {
    match outcome {
        ResumeOutcome::Resumed {
            total_tasks,
            handle,
        } => {
            let run_id = handle.run_id().to_string();
            runs.push(handle);
            Outcome::BuiltinOutput {
                blocks: vec![BlockContent::SystemInfo(format!(
                    "rfc {slug}: dispatch run {run_id} resumed — reconciling {total_tasks} task(s)."
                ))],
            }
        }
        ResumeOutcome::NoRun => Outcome::Error(format!(
            "rfc {slug}: no dispatch run to resume — start one with `orkia rfc dispatch {slug}`"
        )),
        ResumeOutcome::AlreadyClosed { reason } => Outcome::BuiltinOutput {
            blocks: vec![BlockContent::SystemInfo(format!(
                "rfc {slug}: dispatch run already closed ({reason})"
            ))],
        },
        ResumeOutcome::Refused { reason } => {
            Outcome::Error(format!("rfc {slug}: resume refused: {reason}"))
        }
    }
}

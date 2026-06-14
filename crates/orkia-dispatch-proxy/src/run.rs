// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file for terms.

//! The per-run actor (`SPEC-ORKIA-RFC-DISPATCH` §3.3).
//!
//! One [`Driver`] owns all mutable state for a single RFC's dispatch run — the
//! `job_id → task_id` fan-in map and the issue store. It is the *single owner*
//! (CLAUDE.md §2): nothing else touches that state. The `FinalResponseSource`
//! subscription set up by the coordinator fires on the extraction task and
//! must not block, so it only **sends** a [`ProxyMsg`] into the actor's
//! channel; the actor — a dedicated OS thread, never the REPL (§1) — is the
//! sole consumer and drives the run:
//!
//! ```text
//! process first wave ─▶ recv Response ─▶ issue → Done ─▶ kernel.advance
//!        ▲                                                  │
//!        └──────────── process next wave ◀── NextWave ◀─────┘
//!                                         Completed/Paused/Aborted ─▶ close run
//! ```
//!
//! The issue store is the single source of truth: each task's issue at
//! `<rfc_dir>/issues/<id>.md` carries its status, composed prompt, and (once
//! finished) its captured response. **Processing a wave reconciles each task
//! against its issue on disk** — so the same code path drives a fresh run
//! (every issue `Pending` → spawn) and a reconstructed one after a daemon
//! restart (`Done` → fast-forward the kernel, live `Spawned` → re-adopt,
//! lost `Spawned` → fail). Every step is fail-closed (§8): an unresolvable
//! dependency, a write failure, or an unreachable kernel tears the run down
//! rather than letting it limp on with premium behaviour silently assumed.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use orkia_shell_types::dispatch_kernel::{
    DispatchAbortRequest, DispatchAdvanceRequest, DispatchAdvanceResponse, TaskOutcome,
    TaskOutputRef, TaskPlan,
};
use orkia_shell_types::{
    DaemonJobs, DetachedSpawnRequest, DetachedSpawner, FinalResponseEvent, FinalResponseSource,
    KernelRpc, KernelRpcError,
};

use crate::Clock;
use crate::compose::{DepContext, compose_body};
use crate::issues::{Issue, IssueError, IssueMeta, IssueStore, Status, title_from_body};
use crate::seal::{DispatchSeal, SealError};
use crate::spawn::task_command_line;

/// A message into the run actor. The only sender is the coordinator's
/// `FinalResponseSource` callback (one per [`ProxyMsg::Response`]) plus the
/// [`crate::RunHandle::abort`] control path.
pub(crate) enum ProxyMsg {
    /// A final response landed for *some* job. It may not be ours — the
    /// subscription is shell-global — so the actor filters by its fan-in map.
    Response(FinalResponseEvent),
    /// The run was cancelled out-of-band (user `Ctrl-C` / `kill`).
    Abort,
}

/// Whether the actor keeps draining or stops.
enum Flow {
    Continue,
    Done,
}

/// What processing one task in a wave produced.
enum Step {
    /// Spawned, re-adopted, or otherwise resolved with nothing more to do now.
    Continue,
    /// A fast-forward `advance` released more tasks — feed them back in.
    Cascade(Vec<TaskPlan>),
    /// The run reached a terminal verdict; stop the actor.
    Stop,
}

/// The kernel's verdict on one `advance`, normalized for the actor.
enum Advanced {
    Wave(Vec<TaskPlan>),
    Stop,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DriverError {
    #[error(transparent)]
    Issue(#[from] IssueError),
    #[error("spawn detached task `{task_id}`: {reason}")]
    Spawn { task_id: String, reason: String },
    #[error("task `{task_id}` depends on `{dep}`, whose response was never captured")]
    MissingDep { task_id: String, dep: String },
    #[error("kernel advance: {0}")]
    Kernel(#[from] KernelRpcError),
    #[error("seal task `{task_id}`: {source}")]
    Seal {
        task_id: String,
        #[source]
        source: SealError,
    },
}

/// Everything the coordinator hands the actor to build a [`Driver`].
pub(crate) struct DriverConfig {
    pub kernel: Arc<dyn KernelRpc>,
    pub spawner: Arc<dyn DetachedSpawner>,
    pub responses: Arc<dyn FinalResponseSource>,
    pub daemon_jobs: Arc<dyn DaemonJobs>,
    pub run_id: String,
    pub rfc_id: String,
    /// Already-resolved scope name, baked into the `dispatch-task` line.
    pub project: String,
    /// Directory holding the RFC; the issue store lives in `<rfc_dir>/issues/`
    /// and the dispatch SEAL chain in `<rfc_dir>/seal/`.
    pub rfc_dir: PathBuf,
    /// Agent cwd for spawned jobs (`None` → the daemon's default).
    pub working_dir: Option<String>,
    /// Timestamp source, seam-injected so tests are deterministic. Stamps each
    /// sealed output's `ts`.
    pub clock: Clock,
}

/// Single owner of one run's mutable state.
pub(crate) struct Driver {
    kernel: Arc<dyn KernelRpc>,
    spawner: Arc<dyn DetachedSpawner>,
    responses: Arc<dyn FinalResponseSource>,
    daemon_jobs: Arc<dyn DaemonJobs>,
    store: IssueStore,
    /// Tamper-evident audit anchor for finished tasks (§6). Sole writer (§2).
    seal: DispatchSeal,
    clock: Clock,
    run_id: String,
    rfc_id: String,
    project: String,
    working_dir: Option<String>,
    /// `job_id → task_id`: routes a global final-response back to its task.
    job_to_task: HashMap<u32, String>,
}

impl Driver {
    pub(crate) fn new(cfg: DriverConfig) -> Self {
        let store = IssueStore::new(&cfg.rfc_dir);
        let seal = DispatchSeal::new(&cfg.rfc_dir);
        Self {
            kernel: cfg.kernel,
            spawner: cfg.spawner,
            responses: cfg.responses,
            daemon_jobs: cfg.daemon_jobs,
            store,
            seal,
            clock: cfg.clock,
            run_id: cfg.run_id,
            rfc_id: cfg.rfc_id,
            project: cfg.project,
            working_dir: cfg.working_dir,
            job_to_task: HashMap::new(),
        }
    }

    /// Process a wave the kernel says is ready, reconciling each task against
    /// its issue on disk. A `Done`/`Failed` issue fast-forwards the kernel and
    /// may release a cascade of further tasks (drained iteratively here, not
    /// recursively). Fails closed on the first error — the caller tears the
    /// run down rather than driving a partial wave.
    fn process_wave(&mut self, wave: Vec<TaskPlan>) -> Result<Flow, DriverError> {
        let mut queue: VecDeque<TaskPlan> = wave.into_iter().collect();
        while let Some(plan) = queue.pop_front() {
            match self.process_one(plan)? {
                Step::Continue => {}
                Step::Cascade(more) => queue.extend(more),
                Step::Stop => return Ok(Flow::Done),
            }
        }
        Ok(Flow::Continue)
    }

    /// Reconcile one task against its issue: spawn it if it has not run, adopt
    /// or fail a previously-`Spawned` one on resume, or fast-forward the kernel
    /// for one already `Done`/`Failed` before the restart.
    fn process_one(&mut self, plan: TaskPlan) -> Result<Step, DriverError> {
        match self.store.read(&plan.task_id)? {
            None => {
                self.spawn_one(plan)?;
                Ok(Step::Continue)
            }
            Some(issue) => match issue.meta.status {
                Status::Pending => {
                    self.spawn_one(plan)?;
                    Ok(Step::Continue)
                }
                Status::Spawned => self.reconcile_spawned(plan, issue),
                Status::Done => {
                    let outcome = done_outcome(&self.store, &issue);
                    self.fast_forward(&plan.task_id, outcome)
                }
                Status::Failed => {
                    let reason = issue.response.unwrap_or_else(|| "failed".into());
                    self.fast_forward(&plan.task_id, TaskOutcome::Failed { reason })
                }
            },
        }
    }

    /// Compose one task's body from its dependencies' embedded responses, fan
    /// it out as a detached agent job, then write its issue as `Spawned` with
    /// the composed prompt and `job_id`. A crash between the spawn and the
    /// write replays as "still pending" and reconstruction re-spawns — the
    /// same liveness-checked window the design accepts.
    fn spawn_one(&mut self, plan: TaskPlan) -> Result<(), DriverError> {
        let deps = self.dep_contexts(&plan)?;
        let composed = compose_body(&plan.body, &deps);
        let command = task_command_line(&self.rfc_id, &plan.task_id, &plan.agent, &self.project);
        let job_id = self
            .spawner
            .spawn_detached(DetachedSpawnRequest {
                command,
                working_dir: self.working_dir.clone(),
                agent_name: Some(plan.agent.clone()),
                extra_env: Vec::new(),
            })
            .map_err(|reason| DriverError::Spawn {
                task_id: plan.task_id.clone(),
                reason,
            })?;
        self.store.write(&Issue {
            meta: IssueMeta {
                id: plan.task_id.clone(),
                title: title_from_body(&plan.body, &plan.task_id),
                agent: plan.agent.clone(),
                depends_on: plan.depends_on.clone(),
                status: Status::Spawned,
                job_id: Some(job_id),
                response_sha: None,
                seal: None,
            },
            prompt: composed,
            response: None,
        })?;
        self.job_to_task.insert(job_id, plan.task_id);
        Ok(())
    }

    /// A task that was `Spawned` before a restart re-appears in a wave. Recover
    /// it in priority order: a response the daemon already captured (the shell
    /// restarted, the daemon kept running) → fold it in; else a still-running
    /// job → re-adopt its `job_id` so its fan-in routes; else it was lost with
    /// the daemon → fail it closed (§8) and let `on_task_fail` decide.
    fn reconcile_spawned(&mut self, plan: TaskPlan, mut issue: Issue) -> Result<Step, DriverError> {
        if let Some(job_id) = issue.meta.job_id {
            if let Some(ev) = self.responses.latest_for_job(job_id) {
                let outcome = self.absorb(&plan.task_id, &ev)?;
                return self.fast_forward(&plan.task_id, outcome);
            }
            if self.job_alive(job_id) {
                self.job_to_task.insert(job_id, plan.task_id);
                return Ok(Step::Continue);
            }
        }
        issue.meta.status = Status::Failed;
        issue.response = Some("lost on restart".into());
        self.store.write(&issue)?;
        self.fast_forward(
            &plan.task_id,
            TaskOutcome::Failed {
                reason: "lost on restart".into(),
            },
        )
    }

    /// Is `job_id` still a live daemon job (present and not yet exited)?
    fn job_alive(&self, job_id: u32) -> bool {
        self.daemon_jobs
            .list()
            .iter()
            .any(|j| j.id == job_id && j.exit_code.is_none())
    }

    /// Resolve a task's `depends_on` ids to their dependencies' embedded
    /// responses, read from the issue store. A missing or unfinished one is
    /// fail-closed: the kernel only releases a task once its deps are `Done`,
    /// so a gap means our state is corrupt, not pending.
    fn dep_contexts(&self, plan: &TaskPlan) -> Result<Vec<DepContext>, DriverError> {
        plan.depends_on
            .iter()
            .map(|dep| {
                let missing = || DriverError::MissingDep {
                    task_id: plan.task_id.clone(),
                    dep: dep.clone(),
                };
                let issue = self.store.read(dep)?.ok_or_else(missing)?;
                let response = issue.response.ok_or_else(missing)?;
                Ok(DepContext {
                    task_id: dep.clone(),
                    agent: issue.meta.agent,
                    response,
                })
            })
            .collect()
    }

    /// Handle one final-response event. Ignores jobs that aren't ours (the
    /// subscription is shell-global). For ours: fold the outcome into the
    /// issue, relay it to the kernel, and process whatever wave it releases.
    fn on_response(&mut self, ev: FinalResponseEvent) -> Flow {
        let Some(task_id) = self.job_to_task.remove(&ev.job_id) else {
            return Flow::Continue;
        };
        match self.handle_response(&task_id, &ev) {
            Ok(flow) => flow,
            Err(e) => self.teardown(&format!("response `{task_id}`: {e}")),
        }
    }

    fn handle_response(
        &mut self,
        task_id: &str,
        ev: &FinalResponseEvent,
    ) -> Result<Flow, DriverError> {
        let outcome = self.absorb(task_id, ev)?;
        match self.advance(task_id, outcome)? {
            Advanced::Stop => Ok(Flow::Done),
            Advanced::Wave(wave) => self.process_wave(wave),
        }
    }

    /// Fold a finished job into its issue and derive the outcome to report. A
    /// `Stop` with no extracted response (`response_path == None`) marks the
    /// issue `Failed`, not `Done` with empty output.
    fn absorb(
        &mut self,
        task_id: &str,
        ev: &FinalResponseEvent,
    ) -> Result<TaskOutcome, DriverError> {
        let mut issue = self
            .store
            .read(task_id)?
            .ok_or_else(|| DriverError::Spawn {
                task_id: task_id.to_string(),
                reason: "issue vanished before its response landed".into(),
            })?;
        match &ev.response_path {
            Some(path) => {
                let text = read_response(path);
                // Seal the output BEFORE claiming `done`: the issue's status is
                // only trustworthy once the audit anchor exists. A seal failure
                // tears the run down rather than writing an unprovable `done`
                // (§8 — audit-write failure aborts).
                let seal_hash = self
                    .seal
                    .seal_output(
                        task_id,
                        &issue.meta.agent,
                        ev.response_sha256.as_deref(),
                        &path.display().to_string(),
                        &(self.clock)(),
                    )
                    .map_err(|source| DriverError::Seal {
                        task_id: task_id.to_string(),
                        source,
                    })?;
                issue.meta.status = Status::Done;
                issue.meta.response_sha = ev.response_sha256.clone();
                issue.meta.seal = Some(seal_hash);
                issue.response = Some(text);
                self.store.write(&issue)?;
                Ok(TaskOutcome::Done {
                    output: TaskOutputRef {
                        path: path.display().to_string(),
                        bytes: ev.response_bytes,
                        via_mcp: false,
                        elapsed_ms: 0,
                    },
                })
            }
            None => {
                let reason = ev.response_preview.clone();
                issue.meta.status = Status::Failed;
                issue.response = Some(reason.clone());
                self.store.write(&issue)?;
                Ok(TaskOutcome::Failed { reason })
            }
        }
    }

    /// Fast-forward the kernel for a task already terminal on disk (resume),
    /// turning its verdict into more work or a stop.
    fn fast_forward(&mut self, task_id: &str, outcome: TaskOutcome) -> Result<Step, DriverError> {
        match self.advance(task_id, outcome)? {
            Advanced::Wave(wave) => Ok(Step::Cascade(wave)),
            Advanced::Stop => Ok(Step::Stop),
        }
    }

    /// Report a task's outcome to the kernel and normalize the verdict. A
    /// terminal verdict closes the run here; `NextWave` is handed back for the
    /// caller to process.
    fn advance(&mut self, task_id: &str, outcome: TaskOutcome) -> Result<Advanced, DriverError> {
        let resp = self.kernel.dispatch_advance(DispatchAdvanceRequest {
            run_id: self.run_id.clone(),
            task_id: task_id.to_string(),
            outcome,
        })?;
        Ok(match resp {
            DispatchAdvanceResponse::NextWave { wave } => Advanced::Wave(wave),
            DispatchAdvanceResponse::Completed { .. } => {
                self.close_run("completed");
                Advanced::Stop
            }
            DispatchAdvanceResponse::Paused { failed } => {
                self.close_run(&format!("paused: {}", first_or(&failed, task_id)));
                Advanced::Stop
            }
            DispatchAdvanceResponse::Aborted { failed } => {
                self.close_run(&format!("aborted: {failed}"));
                self.abort_kernel();
                Advanced::Stop
            }
            DispatchAdvanceResponse::Failed { reason } => {
                // Kernel could not fold the report (unknown / closed run).
                self.close_run(&format!("kernel: {reason}"));
                Advanced::Stop
            }
        })
    }

    /// External cancel: close the run and tell the kernel to drop state.
    fn on_abort(&mut self) -> Flow {
        self.close_run("cancelled");
        self.abort_kernel();
        Flow::Done
    }

    /// Any unrecoverable error mid-drive: best-effort run close plus a kernel
    /// abort, then stop. Logged, never panicked (§7/§8).
    fn teardown(&self, reason: &str) -> Flow {
        tracing::warn!(run_id = %self.run_id, %reason, "dispatch run torn down");
        self.close_run(reason);
        self.abort_kernel();
        Flow::Done
    }

    /// Mark the run terminal by stamping `closed` on its `.run.toml`. The run
    /// is already ending, so a write failure here is only logged — the issue
    /// statuses on disk still tell the true story (§8).
    fn close_run(&self, reason: &str) {
        match self.store.read_run() {
            Ok(Some(mut meta)) => {
                if meta.closed.is_none() {
                    meta.closed = Some(reason.to_string());
                    if let Err(e) = self.store.write_run(&meta) {
                        tracing::warn!(run_id = %self.run_id, error = %e, "run close write failed");
                    }
                }
            }
            Ok(None) => {
                tracing::warn!(run_id = %self.run_id, "run meta missing at close");
            }
            Err(e) => {
                tracing::warn!(run_id = %self.run_id, error = %e, "run meta unreadable at close");
            }
        }
    }

    /// Best-effort `kernel.v1.dispatch.abort`; the run is already ending so a
    /// failure here is only logged.
    fn abort_kernel(&self) {
        if let Err(e) = self.kernel.dispatch_abort(DispatchAbortRequest {
            run_id: self.run_id.clone(),
        }) {
            tracing::warn!(run_id = %self.run_id, error = %e, "dispatch abort RPC failed");
        }
    }
}

/// Synthesize the `Done` outcome for a task recovered from disk on resume: the
/// kernel only needs to know it finished (to release dependents), so the ref
/// points at the issue file and sizes by the embedded response.
fn done_outcome(store: &IssueStore, issue: &Issue) -> TaskOutcome {
    let bytes = issue.response.as_ref().map(|r| r.len() as u64).unwrap_or(0);
    TaskOutcome::Done {
        output: TaskOutputRef {
            path: store.issue_path(&issue.meta.id).display().to_string(),
            bytes,
            via_mcp: false,
            elapsed_ms: 0,
        },
    }
}

/// Read a captured response file as text, defensively (§7): a non-UTF-8 or
/// unreadable response degrades to a lossy / empty string rather than failing
/// the whole run — the `Stop` hook already vouched the task produced output.
fn read_response(path: &std::path::Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "response file unreadable");
            String::new()
        }
    }
}

/// First failed id the kernel reported, or the task we just advanced when the
/// kernel named none — the run close always points at *some* concrete task.
fn first_or(failed: &[String], fallback: &str) -> String {
    failed
        .first()
        .cloned()
        .unwrap_or_else(|| fallback.to_string())
}

/// The actor loop: process the first wave, then drain messages until a terminal
/// verdict. Runs on a dedicated OS thread (blocking kernel RPC + file I/O),
/// never on the REPL or an async runtime worker. The first wave may itself be
/// terminal — a resume whose every task already finished completes here.
pub(crate) fn run_actor(
    mut driver: Driver,
    first_wave: Vec<TaskPlan>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ProxyMsg>,
) {
    match driver.process_wave(first_wave) {
        Ok(Flow::Done) => return,
        Ok(Flow::Continue) => {}
        Err(e) => {
            driver.teardown(&format!("initial wave: {e}"));
            return;
        }
    }
    while let Some(msg) = rx.blocking_recv() {
        let flow = match msg {
            ProxyMsg::Response(ev) => driver.on_response(ev),
            ProxyMsg::Abort => driver.on_abort(),
        };
        if matches!(flow, Flow::Done) {
            break;
        }
    }
}

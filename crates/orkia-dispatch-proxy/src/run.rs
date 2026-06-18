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
    DispatchAbortRequest, DispatchAdvanceRequest, DispatchAdvanceResponse, DispatchAuthorizeRequest,
    DispatchAuthorizeResponse, DispatchFinalizeRequest, DispatchFinalizeResponse, TaskOutcome,
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
    /// An acceptance oracle finished off-actor (SPEC-CONVERGENCE-LOOP-V1). The
    /// actor decides: pass → advance(Done); fail+retry → re-spawn self-repair;
    /// fail+exhausted → advance(Failed).
    Verdict {
        task_id: String,
        attempt: u32,
        result: crate::oracle::AcceptanceResult,
    },
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

/// What [`Driver::finalize_round`] decided once the DAG drained (V2).
enum FleetStep {
    /// Terminal: close the run with this reason (`converged` / `integration
    /// failed` / `oscillating` / `completed`).
    Stop(String),
    /// Re-plan: drive this fresh round's first wave (from a re-authorize).
    Replan(Vec<TaskPlan>),
}

/// One task's acceptance oracle (SPEC-CONVERGENCE-LOOP-V1). Proxy-local: built
/// from the RFC frontmatter, never sent to the kernel.
#[derive(Clone)]
pub(crate) struct AcceptSpec {
    /// The `accept` command (`exit 0` ⇒ the task succeeded).
    pub command: String,
    /// Total attempts allowed (≥ 1). `1` = a single shot (no retry).
    pub max_attempts: u32,
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
    /// Per-task acceptance specs (SPEC-CONVERGENCE-LOOP-V1), keyed by task id.
    /// Empty ⇒ no convergence loop (every task is one-shot).
    pub accept_specs: HashMap<String, AcceptSpec>,
    /// Sender into the actor's own channel, so an off-actor oracle thread can
    /// post its [`ProxyMsg::Verdict`] back to the actor.
    pub verdict_tx: tokio::sync::mpsc::UnboundedSender<ProxyMsg>,
    /// RFC-level integration oracle (SPEC-FLEET-CONVERGENCE-V2): run once the DAG
    /// drains, its verdict sealed as a `GlobalVerdict`. `None` ⇒ no fleet oracle.
    pub global_accept: Option<String>,
    /// Max fleet re-plan rounds on integration failure (`0` ⇒ no re-plan).
    pub max_replans: u32,
    /// The authorize request, kept so a re-plan round can re-authorize the same
    /// DAG with the kernel (the OSS trivial fallback re-runs every task).
    pub authorize_req: DispatchAuthorizeRequest,
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
    /// Per-task acceptance oracles (SPEC-CONVERGENCE-LOOP-V1).
    accept_specs: HashMap<String, AcceptSpec>,
    /// Back-channel for off-actor oracle threads.
    verdict_tx: tokio::sync::mpsc::UnboundedSender<ProxyMsg>,
    /// RFC-level integration oracle (SPEC-FLEET-CONVERGENCE-V2).
    global_accept: Option<String>,
    /// Max fleet re-plan rounds (`0` ⇒ no re-plan).
    max_replans: u32,
    /// Kept for re-authorize on a re-plan round.
    authorize_req: DispatchAuthorizeRequest,
    /// Current fleet round (`0` = first pass; bumped per re-plan).
    round: u32,
    /// Last integration-failure signature, for no-progress (anti-oscillation)
    /// detection: an unchanged failure across rounds stops the loop.
    last_fail_sig: Option<String>,
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
            accept_specs: cfg.accept_specs,
            verdict_tx: cfg.verdict_tx,
            global_accept: cfg.global_accept,
            max_replans: cfg.max_replans,
            authorize_req: cfg.authorize_req,
            round: 0,
            last_fail_sig: None,
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
                // Resume safety (SPEC-CONVERGENCE-LOOP-V1, Phase 6): the response
                // was captured but the oracle verdict never landed before the
                // restart — re-run the oracle (idempotent), don't advance yet.
                Status::Verifying if self.accept_specs.contains_key(&plan.task_id) => {
                    self.launch_oracle(&plan.task_id)?;
                    Ok(Step::Continue)
                }
                // A `Verifying` task whose `accept` was removed since the run
                // started: treat the captured output as done (nothing to verify).
                Status::Verifying => {
                    let outcome = done_outcome(&self.store, &issue);
                    self.fast_forward(&plan.task_id, outcome)
                }
                // Terminal success: a no-oracle finish (`Done`) or a passed
                // oracle (`Verified`) both report `Done` to the kernel.
                Status::Done | Status::Verified => {
                    let outcome = done_outcome(&self.store, &issue);
                    self.fast_forward(&plan.task_id, outcome)
                }
                Status::Failed => {
                    let reason = issue.response.unwrap_or_else(|| "failed".into());
                    self.fast_forward(&plan.task_id, TaskOutcome::Failed { reason })
                }
                Status::Rejected => {
                    let reason = "acceptance oracle rejected after max attempts".into();
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
                // Left None here: the daemon's `handle_spawn` resolves the cage
                // from its `[cage]` config + this request's `agent_name`, so an
                // RFC-dispatched task is caged identically to a REPL `@agent`
                // spawn without the proxy carrying a cage resolver of its own.
                cage_wrapper: None,
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
                attempt: 0,
                verdict_seal: None,
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
        // Convergence gate (SPEC-CONVERGENCE-LOOP-V1): a finished task WITH an
        // acceptance oracle defers the kernel `advance` until the oracle verdict
        // (run off-actor; returns as `ProxyMsg::Verdict`). A `Failed` outcome
        // (the agent produced no response) skips the oracle and advances now.
        if matches!(outcome, TaskOutcome::Done { .. }) && self.accept_specs.contains_key(task_id) {
            self.launch_oracle(task_id)?;
            return Ok(Flow::Continue);
        }
        self.advance_into_flow(task_id, outcome)
    }

    /// Report an outcome to the kernel and turn its verdict into actor flow.
    fn advance_into_flow(
        &mut self,
        task_id: &str,
        outcome: TaskOutcome,
    ) -> Result<Flow, DriverError> {
        match self.advance(task_id, outcome)? {
            Advanced::Stop => Ok(Flow::Done),
            Advanced::Wave(wave) => self.process_wave(wave),
        }
    }

    /// Read a task's issue or fail closed (it must exist by the time we react
    /// to its response/verdict).
    fn read_issue(&self, task_id: &str) -> Result<Issue, DriverError> {
        self.store
            .read(task_id)?
            .ok_or_else(|| DriverError::Spawn {
                task_id: task_id.to_string(),
                reason: "issue vanished before its verdict".into(),
            })
    }

    /// Flip a finished task to `Verifying` and run its `accept` oracle on a
    /// dedicated thread (it may take minutes; the actor must stay responsive).
    /// The result comes back as [`ProxyMsg::Verdict`].
    fn launch_oracle(&mut self, task_id: &str) -> Result<(), DriverError> {
        // Callers guarantee the spec exists; if it somehow doesn't there is
        // nothing to verify, so return without spawning (fail-soft, no panic).
        let Some(spec) = self.accept_specs.get(task_id).cloned() else {
            return Ok(());
        };
        let mut issue = self.read_issue(task_id)?;
        issue.meta.status = Status::Verifying;
        self.store.write(&issue)?;
        let attempt = issue.meta.attempt;
        let working_dir = self.working_dir.clone();
        let tx = self.verdict_tx.clone();
        let tid = task_id.to_string();
        std::thread::spawn(move || {
            let result = crate::oracle::run_acceptance(&spec.command, working_dir.as_deref());
            let _ = tx.send(ProxyMsg::Verdict {
                task_id: tid,
                attempt,
                result,
            });
        });
        Ok(())
    }

    /// An acceptance oracle finished. Seal the verdict, then: pass → `Verified` +
    /// advance `Done`; fail with attempts left → re-spawn self-repair; fail
    /// exhausted → `Rejected` + advance `Failed`. The kernel sees exactly one
    /// outcome per task (after convergence), so retries never reach it.
    fn on_verdict(
        &mut self,
        task_id: &str,
        attempt: u32,
        result: crate::oracle::AcceptanceResult,
    ) -> Flow {
        match self.handle_verdict(task_id, attempt, result) {
            Ok(flow) => flow,
            Err(e) => self.teardown(&format!("verdict `{task_id}`: {e}")),
        }
    }

    fn handle_verdict(
        &mut self,
        task_id: &str,
        attempt: u32,
        result: crate::oracle::AcceptanceResult,
    ) -> Result<Flow, DriverError> {
        let Some(spec) = self.accept_specs.get(task_id).cloned() else {
            return Ok(Flow::Continue); // not an accept task — nothing to do
        };
        let mut issue = self.read_issue(task_id)?;
        // A verdict for a superseded attempt (a later retry already ran) is
        // stale — ignore it rather than double-advance.
        if issue.meta.attempt != attempt {
            return Ok(Flow::Continue);
        }
        let seal_hash = self
            .seal
            .seal_verdict(
                task_id,
                &issue.meta.agent,
                attempt,
                &spec.command,
                result.exit_code,
                result.passed(),
                &(self.clock)(),
            )
            .map_err(|source| DriverError::Seal {
                task_id: task_id.to_string(),
                source,
            })?;
        issue.meta.verdict_seal = Some(seal_hash);

        if result.passed() {
            issue.meta.status = Status::Verified;
            self.store.write(&issue)?;
            let outcome = done_outcome(&self.store, &issue);
            self.advance_into_flow(task_id, outcome)
        } else if attempt + 1 < spec.max_attempts {
            self.store.write(&issue)?; // persist the verdict seal before re-spawn
            self.retry_task(task_id, issue, &spec.command, &result.output_tail)?;
            Ok(Flow::Continue)
        } else {
            issue.meta.status = Status::Rejected;
            self.store.write(&issue)?;
            let reason = format!(
                "acceptance `{}` failed after {} attempt(s) (exit {})",
                spec.command,
                attempt + 1,
                result.exit_code
            );
            self.advance_into_flow(task_id, TaskOutcome::Failed { reason })
        }
    }

    /// Re-spawn a task for another attempt with a self-repair prompt built from
    /// the failing acceptance output. The prior job already finished (its
    /// response was absorbed), so there is no orphan — this is a fresh job in
    /// the same workspace, which sees the previous attempt's changes.
    fn retry_task(
        &mut self,
        task_id: &str,
        mut issue: Issue,
        accept_command: &str,
        output_tail: &str,
    ) -> Result<(), DriverError> {
        let next_attempt = issue.meta.attempt + 1;
        let body = compose_retry_body(&issue.prompt, accept_command, output_tail, next_attempt);
        let command = task_command_line(&self.rfc_id, task_id, &issue.meta.agent, &self.project);
        let job_id = self
            .spawner
            .spawn_detached(DetachedSpawnRequest {
                command,
                working_dir: self.working_dir.clone(),
                agent_name: Some(issue.meta.agent.clone()),
                extra_env: Vec::new(),
                cage_wrapper: None,
            })
            .map_err(|reason| DriverError::Spawn {
                task_id: task_id.to_string(),
                reason,
            })?;
        issue.meta.attempt = next_attempt;
        issue.meta.status = Status::Spawned;
        issue.meta.job_id = Some(job_id);
        issue.meta.response_sha = None;
        issue.prompt = body;
        issue.response = None;
        self.store.write(&issue)?;
        self.job_to_task.insert(job_id, task_id.to_string());
        Ok(())
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
            DispatchAdvanceResponse::Completed { .. } => match self.finalize_round() {
                FleetStep::Stop(reason) => {
                    self.close_run(&reason);
                    Advanced::Stop
                }
                // Re-plan: the kernel re-authorized a fresh round; keep driving.
                FleetStep::Replan(wave) => Advanced::Wave(wave),
            },
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

    /// The DAG has drained: run the RFC-level integration oracle (if any), seal
    /// the round's `GlobalVerdict`, and decide the fleet's next step
    /// (SPEC-FLEET-CONVERGENCE-V2). Pass ⇒ `converged`; fail ⇒ re-plan another
    /// round (bounded by `max_replans` + no-progress detection) or stop.
    ///
    /// OSS controller `(b)` + trivial brain `(a)`: the brain here is "re-run
    /// all" via re-authorize (the kernel re-schedules the same DAG). A premium
    /// kernel brain would instead return a TARGETED re-dispatch + amend the DAG
    /// in place — it slots in where `replan_rerun_all` is called.
    ///
    /// Runs synchronously: the DAG is drained, so no task is in flight to starve.
    fn finalize_round(&mut self) -> FleetStep {
        let Some(cmd) = self.global_accept.clone() else {
            // No integration gate: DAG completion is the result. Still finalize
            // so the kernel drops the (now kept-alive) run.
            self.notify_kernel_converged();
            return FleetStep::Stop("completed".to_string());
        };
        let result = crate::oracle::run_acceptance(&cmd, self.working_dir.as_deref());
        if let Err(e) =
            self.seal
                .seal_global_verdict(self.round, &cmd, result.exit_code, result.passed(), &(self.clock)())
        {
            tracing::warn!(run_id = %self.run_id, error = %e, "global verdict seal failed");
        }
        if result.passed() {
            self.notify_kernel_converged();
            return FleetStep::Stop("converged".to_string());
        }
        // Integration failed — consider a re-plan round.
        if self.round >= self.max_replans {
            return FleetStep::Stop(format!(
                "integration failed: `{cmd}` exit {} (after {} re-plan(s))",
                result.exit_code, self.round
            ));
        }
        let sig = failure_signature(&result.output_tail);
        if self.last_fail_sig.as_deref() == Some(sig.as_str()) {
            // The failure is unchanged since the last round → no progress; stop
            // rather than thrash (anti-oscillation).
            let _ = self
                .seal
                .seal_replan_decision(self.round, "give-up: no progress", &(self.clock)());
            return FleetStep::Stop("oscillating: integration failure unchanged".to_string());
        }
        self.last_fail_sig = Some(sig);
        self.replan(result.output_tail)
    }

    /// Tell the kernel the run converged so it drops the kept-alive run (V2 inc
    /// 3). Best-effort: a kernel without the finalize RPC already dropped the
    /// run on completion and returns Unavailable — harmless.
    fn notify_kernel_converged(&self) {
        let _ = self.kernel.dispatch_finalize(DispatchFinalizeRequest {
            run_id: self.run_id.clone(),
            passed: true,
            round: self.round,
            failure_tail: None,
        });
    }

    /// Re-plan one round. Prefer the kernel's TARGETED re-open (premium brain):
    /// it re-opens only the affected subgraph and returns its wave (same
    /// run_id). Fall back to re-authorizing the whole DAG (the OSS trivial
    /// brain) when the kernel lacks the finalize RPC. Either way `round`
    /// advances and a `ReplanDecision` is sealed.
    fn replan(&mut self, failure_tail: String) -> FleetStep {
        let finalize = self.kernel.dispatch_finalize(DispatchFinalizeRequest {
            run_id: self.run_id.clone(),
            passed: false,
            round: self.round,
            failure_tail: Some(failure_tail),
        });
        match finalize {
            Ok(DispatchFinalizeResponse::Replan { wave, reopened }) => {
                let _ =
                    self.seal
                        .seal_replan_decision(self.round, "rerun-targeted", &(self.clock)());
                // Same run_id (the kernel kept the run). Reset the FULL re-opened
                // closure — the wave PLUS its downstream dependents released in
                // later waves — so dependents re-run instead of fast-forwarding
                // their stale output (a wave-only reset skips them).
                for id in &reopened {
                    if let Err(e) = self.reset_issue_for_replan(id) {
                        return FleetStep::Stop(format!("re-plan reset failed: {e}"));
                    }
                }
                self.round += 1;
                FleetStep::Replan(wave)
            }
            Ok(DispatchFinalizeResponse::GiveUp { reason }) => {
                FleetStep::Stop(format!("re-plan declined: {reason}"))
            }
            Ok(DispatchFinalizeResponse::Converged) => FleetStep::Stop("converged".to_string()),
            // No finalize RPC (old kernel) or a transport error ⇒ OSS fallback:
            // re-authorize the whole DAG.
            _ => {
                if let Err(e) =
                    self.seal
                        .seal_replan_decision(self.round, "rerun-all", &(self.clock)())
                {
                    tracing::warn!(run_id = %self.run_id, error = %e, "replan decision seal failed");
                }
                match self.replan_rerun_all() {
                    Ok(wave) => {
                        self.round += 1;
                        FleetStep::Replan(wave)
                    }
                    Err(e) => FleetStep::Stop(format!("re-plan failed: {e}")),
                }
            }
        }
    }

    /// The OSS trivial re-plan: reset every task to `Pending` and re-authorize
    /// the same DAG with the kernel, returning its fresh first wave. The agents
    /// re-run in the SAME workspace, so each round sees the prior round's
    /// changes (fleet-scale self-repair). Per-task `accept` oracles re-converge
    /// each task within the round.
    fn replan_rerun_all(&mut self) -> Result<Vec<TaskPlan>, DriverError> {
        let resp = self.kernel.dispatch_authorize(self.authorize_req.clone())?;
        let wave = match resp {
            DispatchAuthorizeResponse::Authorized { run_id, wave, .. } => {
                self.run_id = run_id;
                wave
            }
            DispatchAuthorizeResponse::Refused { reason } => {
                return Err(DriverError::Spawn {
                    task_id: "<re-plan>".to_string(),
                    reason: format!("kernel refused re-authorize: {reason}"),
                });
            }
        };
        // Re-stamp the run identity with the new round's run_id (advance keys on
        // it); keep the original `started`.
        if let Ok(Some(mut meta)) = self.store.read_run() {
            meta.run_id = self.run_id.clone();
            if let Err(e) = self.store.write_run(&meta) {
                tracing::warn!(run_id = %self.run_id, error = %e, "re-plan run-meta rewrite failed");
            }
        }
        // Reset every task to `Pending`; `process_wave` re-spawns from the wave.
        for task in &self.authorize_req.tasks.clone() {
            self.reset_issue_for_replan(&task.id)?;
        }
        Ok(wave)
    }

    /// Reset one task's issue to `Pending` for a fresh re-plan round: clear the
    /// job, response, attempt, and per-task seals; keep `id`/`agent`/`deps`.
    fn reset_issue_for_replan(&self, task_id: &str) -> Result<(), DriverError> {
        if let Some(mut issue) = self.store.read(task_id)? {
            issue.meta.status = Status::Pending;
            issue.meta.job_id = None;
            issue.meta.response_sha = None;
            issue.meta.seal = None;
            issue.meta.attempt = 0;
            issue.meta.verdict_seal = None;
            issue.response = None;
            self.store.write(&issue)?;
        }
        Ok(())
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

/// A stable signature of an integration-failure output, for no-progress
/// (anti-oscillation) detection across re-plan rounds. Hashes the trimmed tail:
/// a byte-identical failure two rounds running means the re-plan achieved
/// nothing, so the loop stops rather than thrash.
fn failure_signature(output_tail: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(output_tail.trim().as_bytes());
    hex::encode(h.finalize())
}

/// Build a self-repair prompt for a retry (SPEC-CONVERGENCE-LOOP-V1, Phase 5):
/// the prior prompt plus the failing acceptance command and its (bounded)
/// output, so the agent fixes the cause rather than starting blind.
fn compose_retry_body(
    prior_prompt: &str,
    accept_command: &str,
    output_tail: &str,
    attempt: u32,
) -> String {
    format!(
        "{prior_prompt}\n\n\
         ---\n\
         Attempt {attempt}: the acceptance check `{accept_command}` failed. Its output was:\n\
         ```\n{output_tail}\n```\n\
         Fix the underlying cause so `{accept_command}` passes, then stop."
    )
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
            ProxyMsg::Verdict {
                task_id,
                attempt,
                result,
            } => driver.on_verdict(&task_id, attempt, result),
            ProxyMsg::Abort => driver.on_abort(),
        };
        if matches!(flow, Flow::Done) {
            break;
        }
    }
}

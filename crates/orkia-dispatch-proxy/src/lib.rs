// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file for terms.

//! `KernelDispatchProxy` — the OSS side of RFC → many-agents dispatch.
//!
//! In the single-shell architecture the DAG **brain** lives in the
//! `orkia-kernel` daemon (Team-gated); the shell owns task execution. This
//! crate is the seam, the exact sibling of `orkia-pipeline-proxy` for
//! `@a | @b`, generalized from a linear pipeline to a wave-scheduled DAG:
//!
//! 1. Resolve every `@agent → command` itself (the kernel never reads the
//!    agent registry), then ask the kernel to `authorize` the plan. The
//!    kernel validates structure + policy and returns the first **wave** of
//!    ready task plans, or a refusal.
//! 2. Persist the run: write the run identity and one `Pending` issue per task
//!    to `<rfc_dir>/issues/` ([`issues`]) *before* any task spawns (fail-closed
//!    on a write error). Each issue is the single source of truth for its task.
//! 3. For each task in a wave: compose its prompt from its dependencies'
//!    embedded responses ([`compose`]), write the issue as `Spawned`, and fan
//!    it out as a detached agent job via [`DetachedSpawner`].
//! 4. Fan-in: a single [`FinalResponseSource`] subscription matches each
//!    `job_id` to its task, embeds the captured response into the issue,
//!    relays the outcome to the kernel's `advance`, and spawns whatever new
//!    wave the kernel returns — until the kernel reports
//!    `Completed` / `Aborted` / `Paused`.
//!
//! Durability: the issues directory *is* the run state, so a daemon restart
//! reconstructs in-flight runs by scanning it (reconstruction lands in a
//! follow-up). Fail-closed throughout: an unreachable kernel, an unresolvable
//! agent, a kernel refusal, or an issue-write failure all stop the run rather
//! than silently assuming premium behaviour.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod compose;
pub mod issues;
mod run;
pub mod seal;
pub mod spawn;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::Arc;

use orkia_shell_types::dispatch_kernel::{
    DispatchAuthorizeRequest, DispatchAuthorizeResponse, DispatchTaskRequest, TaskPlan,
};
use orkia_shell_types::{DaemonJobs, DetachedSpawner, FinalResponseSource, KernelRpc};
use sha2::{Digest, Sha256};

use crate::issues::{Issue, IssueMeta, IssueStore, RunMeta, Status, title_from_body};
use crate::run::{Driver, DriverConfig, ProxyMsg, run_actor};

pub use compose::{DepContext, compose_body};
pub use seal::{DispatchSeal, DispatchSealRecord};
pub use spawn::task_command_line;

/// Injected wall-clock: returns an RFC3339 timestamp for the run's `started`
/// stamp. The crate never reads the clock itself so runs stay deterministic in
/// tests and the caller owns time (the command surface passes `chrono::Utc::now`).
pub type Clock = Arc<dyn Fn() -> String + Send + Sync>;

/// The runtime the shell resolved for one agent task. Mirrors the fields the
/// kernel needs in [`DispatchTaskRequest`]; resolution stays OSS-side.
#[derive(Clone, Debug)]
pub struct ResolvedRuntime {
    pub command: String,
    pub args: Vec<String>,
    pub provider: Option<String>,
    /// `Some(model_ref)` when the agent is `[runtime] type = "native"`.
    pub runtime: Option<String>,
}

/// Maps an agent name to its runtime. Implemented in `bins/orkia` over the
/// agent directory, kept behind a trait so this crate stays free of the
/// agent-loading machinery. Sibling of `orkia-pipeline-proxy::StageResolver`.
pub trait AgentResolver: Send + Sync {
    fn resolve(&self, agent: &str) -> Option<ResolvedRuntime>;
}

/// One task as the RFC's `[dispatch]` block declares it (pre-resolution).
#[derive(Clone, Debug)]
pub struct DispatchTaskSpec {
    pub id: String,
    pub agent: String,
    pub body: String,
    pub depends_on: Vec<String>,
}

/// A whole dispatch run the command surface asks the proxy to start.
pub struct DispatchRequest {
    pub rfc_id: String,
    /// Already-resolved scope name, baked into each `dispatch-task` line.
    pub project: String,
    /// Directory holding the RFC; the issues store lives in `<rfc_dir>/issues/`.
    pub rfc_dir: PathBuf,
    /// Agent cwd for spawned jobs (`None` → the daemon's default).
    pub working_dir: Option<String>,
    pub strategy: String,
    pub max_inflight: usize,
    pub on_task_fail: String,
    pub tasks: Vec<DispatchTaskSpec>,
}

/// A live run's control handle, returned on a successful start. The command
/// surface holds it to cancel the run (`Ctrl-C` / `kill`).
pub struct RunHandle {
    run_id: String,
    tx: tokio::sync::mpsc::UnboundedSender<ProxyMsg>,
}

impl RunHandle {
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Ask the actor to tear the run down. Idempotent and non-blocking; a
    /// dropped receiver (actor already finished) is ignored.
    pub fn abort(&self) {
        let _ = self.tx.send(ProxyMsg::Abort);
    }
}

/// Outcome of [`KernelDispatchProxy::start_run`].
pub enum DispatchStartOutcome {
    /// The run was authorized and its actor is driving the first wave.
    Started { total_tasks: u32, handle: RunHandle },
    /// Rejected before any task spawned (unresolvable agent, unreachable
    /// kernel, kernel refusal, or an issue-write failure).
    Refused { reason: String },
}

/// Outcome of [`KernelDispatchProxy::resume_run`].
pub enum ResumeOutcome {
    /// A non-terminal run was found and its actor is reconciling it.
    Resumed { total_tasks: u32, handle: RunHandle },
    /// No `.run.toml` for this RFC — nothing to resume (start fresh instead).
    NoRun,
    /// The run already finished; resuming is refused (`closed` carries why).
    AlreadyClosed { reason: String },
    /// Could not resume (unresolvable agent, unreachable kernel, kernel
    /// refusal, or an unreadable/unwritable run meta).
    Refused { reason: String },
}

/// OSS dispatch coordinator. Cheap to construct; holds cloneable handles.
pub struct KernelDispatchProxy {
    kernel: Arc<dyn KernelRpc>,
    resolver: Arc<dyn AgentResolver>,
    spawner: Arc<dyn DetachedSpawner>,
    responses: Arc<dyn FinalResponseSource>,
    daemon_jobs: Arc<dyn DaemonJobs>,
    clock: Clock,
}

impl KernelDispatchProxy {
    pub fn new(
        kernel: Arc<dyn KernelRpc>,
        resolver: Arc<dyn AgentResolver>,
        seams: DispatchSeams,
    ) -> Self {
        Self {
            kernel,
            resolver,
            spawner: seams.spawner,
            responses: seams.responses,
            daemon_jobs: seams.daemon_jobs,
            clock: seams.clock,
        }
    }

    /// Resolve agents, authorize the plan with the kernel, persist the run as
    /// issues, and — on success — start the run actor on a dedicated thread.
    /// Returns immediately; the caller is back at the prompt while the run
    /// drives itself (CLAUDE.md §1).
    pub fn start_run(&self, req: DispatchRequest) -> DispatchStartOutcome {
        let tasks = match resolve_tasks(&req.tasks, &*self.resolver) {
            Ok(t) => t,
            Err(reason) => return DispatchStartOutcome::Refused { reason },
        };
        let authorize = DispatchAuthorizeRequest {
            strategy: req.strategy.clone(),
            max_inflight: req.max_inflight,
            on_task_fail: req.on_task_fail.clone(),
            tasks,
        };
        let (run_id, total_tasks, wave) = match self.kernel.dispatch_authorize(authorize.clone()) {
            Ok(DispatchAuthorizeResponse::Authorized {
                run_id,
                total_tasks,
                wave,
            }) => (run_id, total_tasks, wave),
            Ok(DispatchAuthorizeResponse::Refused { reason }) => {
                return DispatchStartOutcome::Refused { reason };
            }
            Err(e) => {
                return DispatchStartOutcome::Refused {
                    reason: format!("kernel unavailable: {e}"),
                };
            }
        };
        if let Err(reason) = self.open_run(&req, &run_id, &authorize) {
            return DispatchStartOutcome::Refused { reason };
        }
        let handle = self.launch(req, run_id, wave);
        DispatchStartOutcome::Started {
            total_tasks,
            handle,
        }
    }

    /// Resume a run left on disk after a daemon restart (`SPEC` §4.3). The
    /// issues store *is* the run state, so there is no ledger to replay: the
    /// caller rebuilds the same [`DispatchRequest`] from the RFC on disk, and
    /// the actor reconciles the freshly-authorized waves against each issue's
    /// status (skip `Done`, re-adopt live `Spawned`, fail the lost ones). The
    /// kernel's DAG state is in-memory and gone, so the plan is re-`authorize`d
    /// from scratch and fast-forwarded over the already-finished tasks.
    pub fn resume_run(&self, req: DispatchRequest) -> ResumeOutcome {
        let store = IssueStore::new(&req.rfc_dir);
        let prior = match store.read_run() {
            Ok(Some(meta)) => meta,
            Ok(None) => return ResumeOutcome::NoRun,
            Err(e) => {
                return ResumeOutcome::Refused {
                    reason: format!("run meta unreadable: {e}"),
                };
            }
        };
        if let Some(reason) = prior.closed {
            return ResumeOutcome::AlreadyClosed { reason };
        }
        let tasks = match resolve_tasks(&req.tasks, &*self.resolver) {
            Ok(t) => t,
            Err(reason) => return ResumeOutcome::Refused { reason },
        };
        let authorize = DispatchAuthorizeRequest {
            strategy: req.strategy.clone(),
            max_inflight: req.max_inflight,
            on_task_fail: req.on_task_fail.clone(),
            tasks,
        };
        let (run_id, total_tasks, wave) = match self.kernel.dispatch_authorize(authorize.clone()) {
            Ok(DispatchAuthorizeResponse::Authorized {
                run_id,
                total_tasks,
                wave,
            }) => (run_id, total_tasks, wave),
            Ok(DispatchAuthorizeResponse::Refused { reason }) => {
                return ResumeOutcome::Refused { reason };
            }
            Err(e) => {
                return ResumeOutcome::Refused {
                    reason: format!("kernel unavailable: {e}"),
                };
            }
        };
        // Re-stamp identity with the kernel's fresh run_id (the actor keys
        // `advance` by it), keeping the original `started`. The issues are left
        // untouched — they are the state the resume reconciles against.
        let restamped = RunMeta {
            run_id: run_id.clone(),
            plan_hash: plan_hash(&authorize),
            strategy: req.strategy.clone(),
            started: prior.started,
            closed: None,
        };
        if let Err(e) = store.write_run(&restamped) {
            return ResumeOutcome::Refused {
                reason: format!("run meta unwritable: {e}"),
            };
        }
        let handle = self.launch(req, run_id, wave);
        ResumeOutcome::Resumed {
            total_tasks,
            handle,
        }
    }

    /// Write the run identity and one `Pending` issue per task before any task
    /// spawns. A write failure here refuses the run (§8) rather than driving a
    /// run whose source of truth is missing.
    fn open_run(
        &self,
        req: &DispatchRequest,
        run_id: &str,
        authorize: &DispatchAuthorizeRequest,
    ) -> Result<(), String> {
        let store = IssueStore::new(&req.rfc_dir);
        store
            .write_run(&RunMeta {
                run_id: run_id.to_string(),
                plan_hash: plan_hash(authorize),
                strategy: req.strategy.clone(),
                started: (self.clock)(),
                closed: None,
            })
            .map_err(|e| format!("dispatch run unwritable: {e}"))?;
        for spec in &req.tasks {
            store
                .write(&Issue {
                    meta: IssueMeta {
                        id: spec.id.clone(),
                        title: title_from_body(&spec.body, &spec.id),
                        agent: spec.agent.clone(),
                        depends_on: spec.depends_on.clone(),
                        status: Status::Pending,
                        job_id: None,
                        response_sha: None,
                        seal: None,
                    },
                    prompt: spec.body.clone(),
                    response: None,
                })
                .map_err(|e| format!("issue `{}` unwritable: {e}", spec.id))?;
        }
        Ok(())
    }

    /// Build the driver, wire the fan-in subscription, and start the actor.
    fn launch(&self, req: DispatchRequest, run_id: String, wave: Vec<TaskPlan>) -> RunHandle {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let driver = Driver::new(DriverConfig {
            kernel: Arc::clone(&self.kernel),
            spawner: Arc::clone(&self.spawner),
            responses: Arc::clone(&self.responses),
            daemon_jobs: Arc::clone(&self.daemon_jobs),
            run_id: run_id.clone(),
            rfc_id: req.rfc_id,
            project: req.project,
            rfc_dir: req.rfc_dir,
            working_dir: req.working_dir,
            clock: Arc::clone(&self.clock),
        });
        let sink = tx.clone();
        self.responses.subscribe(Arc::new(move |ev| {
            let _ = sink.send(ProxyMsg::Response(ev));
        }));
        std::thread::spawn(move || run_actor(driver, wave, rx));
        RunHandle { run_id, tx }
    }
}

/// The execution seams the proxy needs beyond the kernel + resolver. Grouped
/// so [`KernelDispatchProxy::new`] keeps to four arguments. `daemon_jobs` is
/// only exercised on resume (liveness of a pre-restart `Spawned` task).
pub struct DispatchSeams {
    pub spawner: Arc<dyn DetachedSpawner>,
    pub responses: Arc<dyn FinalResponseSource>,
    pub daemon_jobs: Arc<dyn DaemonJobs>,
    pub clock: Clock,
}

/// Resolve every task to a [`DispatchTaskRequest`]. Fails closed on the first
/// agent the shell can't resolve — before the kernel is contacted.
fn resolve_tasks(
    specs: &[DispatchTaskSpec],
    resolver: &dyn AgentResolver,
) -> Result<Vec<DispatchTaskRequest>, String> {
    specs.iter().map(|s| resolve_one(s, resolver)).collect()
}

fn resolve_one(
    spec: &DispatchTaskSpec,
    resolver: &dyn AgentResolver,
) -> Result<DispatchTaskRequest, String> {
    let rt = resolver.resolve(&spec.agent).ok_or_else(|| {
        format!(
            "task `{}`: agent @{} has no command configured",
            spec.id, spec.agent
        )
    })?;
    Ok(DispatchTaskRequest {
        id: spec.id.clone(),
        agent: spec.agent.clone(),
        body: spec.body.clone(),
        command: rt.command,
        args: rt.args,
        provider: rt.provider,
        runtime: rt.runtime,
        depends_on: spec.depends_on.clone(),
    })
}

/// Provenance anchor for the run: a stable digest of the authorized plan.
/// Truncated to 16 hex chars, matching the final-response sha convention.
fn plan_hash(req: &DispatchAuthorizeRequest) -> String {
    let bytes = serde_json::to_vec(req).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    hex::encode(hasher.finalize())[..16].to_string()
}

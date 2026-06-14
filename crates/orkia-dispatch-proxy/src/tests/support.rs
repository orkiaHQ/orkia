// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file for terms.

//! Shared fakes + helpers for the actor tests.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use orkia_shell_types::dispatch_kernel::{
    DispatchAbortRequest, DispatchAbortResponse, DispatchAdvanceRequest, DispatchAdvanceResponse,
    DispatchAuthorizeRequest, DispatchAuthorizeResponse, TaskPlan,
};
use orkia_shell_types::{
    DaemonJobView, DaemonJobs, DetachedSpawnRequest, DetachedSpawner, FinalResponseCallback,
    FinalResponseEvent, FinalResponseSource, IntentGuess, KernelRpc, KernelRpcError, KernelVersion,
};

use crate::issues::{Issue, IssueMeta, IssueStore, RunMeta, Status};
use crate::{
    AgentResolver, DispatchRequest, DispatchSeams, DispatchTaskSpec, KernelDispatchProxy,
    ResolvedRuntime,
};

// ── Fakes ────────────────────────────────────────────────────────────

pub(crate) struct FakeKernel {
    authorize: Mutex<Option<DispatchAuthorizeResponse>>,
    advances: Mutex<VecDeque<DispatchAdvanceResponse>>,
    pub advance_log: Mutex<Vec<String>>,
    pub aborts: Mutex<u32>,
}

impl FakeKernel {
    pub(crate) fn new(
        authorize: DispatchAuthorizeResponse,
        advances: Vec<DispatchAdvanceResponse>,
    ) -> Arc<Self> {
        Arc::new(Self {
            authorize: Mutex::new(Some(authorize)),
            advances: Mutex::new(advances.into()),
            advance_log: Mutex::new(Vec::new()),
            aborts: Mutex::new(0),
        })
    }
}

impl KernelRpc for FakeKernel {
    fn version(&self) -> KernelVersion {
        KernelVersion {
            protocol: 1,
            kernel: "fake".into(),
            min_client: None,
            capabilities: Vec::new(),
        }
    }

    fn classify_with_timeout(
        &self,
        _line: &str,
        _timeout: Duration,
    ) -> Result<IntentGuess, KernelRpcError> {
        Err(KernelRpcError::Unavailable("test".into()))
    }

    fn shutdown(&self) -> Result<(), KernelRpcError> {
        Ok(())
    }

    fn dispatch_authorize(
        &self,
        _req: DispatchAuthorizeRequest,
    ) -> Result<DispatchAuthorizeResponse, KernelRpcError> {
        match self.authorize.lock().unwrap().take() {
            Some(r) => Ok(r),
            None => Err(KernelRpcError::Unavailable(
                "authorize already taken".into(),
            )),
        }
    }

    fn dispatch_advance(
        &self,
        req: DispatchAdvanceRequest,
    ) -> Result<DispatchAdvanceResponse, KernelRpcError> {
        self.advance_log.lock().unwrap().push(req.task_id);
        match self.advances.lock().unwrap().pop_front() {
            Some(r) => Ok(r),
            None => Ok(DispatchAdvanceResponse::Completed { elapsed_ms: 0 }),
        }
    }

    fn dispatch_abort(
        &self,
        _req: DispatchAbortRequest,
    ) -> Result<DispatchAbortResponse, KernelRpcError> {
        *self.aborts.lock().unwrap() += 1;
        Ok(DispatchAbortResponse { ok: true })
    }
}

#[derive(Default)]
pub(crate) struct FakeSpawner {
    pub spawned: Mutex<Vec<DetachedSpawnRequest>>,
}

impl FakeSpawner {
    pub(crate) fn count(&self) -> usize {
        self.spawned.lock().unwrap().len()
    }
}

impl DetachedSpawner for FakeSpawner {
    fn spawn_detached(&self, req: DetachedSpawnRequest) -> Result<u32, String> {
        let mut g = self.spawned.lock().unwrap();
        g.push(req);
        Ok(g.len() as u32) // job ids 1, 2, 3, …
    }
}

#[derive(Default)]
pub(crate) struct FakeResponses {
    subs: Mutex<Vec<FinalResponseCallback>>,
    latest: Mutex<HashMap<u32, FinalResponseEvent>>,
}

impl FakeResponses {
    /// Fire a final-response into every subscriber (the daemon's fan-in path).
    pub(crate) fn fire(&self, ev: FinalResponseEvent) {
        let cbs = self.subs.lock().unwrap().clone();
        for cb in cbs {
            cb(ev.clone());
        }
    }

    /// Seed a response the daemon already captured before the shell subscribed
    /// — what `latest_for_job` recovers on resume.
    pub(crate) fn set_latest(&self, job_id: u32, ev: FinalResponseEvent) {
        self.latest.lock().unwrap().insert(job_id, ev);
    }
}

impl FinalResponseSource for FakeResponses {
    fn subscribe(&self, callback: FinalResponseCallback) {
        self.subs.lock().unwrap().push(callback);
    }
    fn latest_for_job(&self, job_id: u32) -> Option<FinalResponseEvent> {
        self.latest.lock().unwrap().get(&job_id).cloned()
    }
}

#[derive(Default)]
pub(crate) struct FakeDaemonJobs {
    jobs: Mutex<Vec<DaemonJobView>>,
}

impl FakeDaemonJobs {
    /// Mark `job_id` as a live (un-exited) daemon job.
    pub(crate) fn add_live(&self, job_id: u32) {
        self.jobs.lock().unwrap().push(DaemonJobView {
            id: job_id,
            agent: "agent".into(),
            state: "running".into(),
            pid: Some(1000 + job_id),
            label: format!("job {job_id}"),
            runtime_secs: 1,
            exit_code: None,
            stages: Vec::new(),
        });
    }
}

impl DaemonJobs for FakeDaemonJobs {
    fn list(&self) -> Vec<DaemonJobView> {
        self.jobs.lock().unwrap().clone()
    }
    fn wait(&self, _id: u32, _t: Duration) -> Result<(String, i32), String> {
        Err("unused".into())
    }
    fn kill(&self, _id: u32) -> Result<(), String> {
        Err("unused".into())
    }
    fn tell(&self, _id: u32, _m: &str) -> Result<(), String> {
        Err("unused".into())
    }
    fn attach(&self, _id: u32) -> Result<(), String> {
        Err("unused".into())
    }
}

pub(crate) struct MapResolver;
impl AgentResolver for MapResolver {
    fn resolve(&self, agent: &str) -> Option<ResolvedRuntime> {
        if agent == "ghost" {
            return None;
        }
        Some(ResolvedRuntime {
            command: "claude".into(),
            args: vec![],
            provider: Some("claude".into()),
            runtime: None,
        })
    }
}

/// The four fakes a test wires into a proxy, kept together so assertions can
/// reach each one after the run.
pub(crate) struct Fakes {
    pub kernel: Arc<FakeKernel>,
    pub spawner: Arc<FakeSpawner>,
    pub responses: Arc<FakeResponses>,
    pub daemon: Arc<FakeDaemonJobs>,
}

impl Fakes {
    pub(crate) fn new(
        authorize: DispatchAuthorizeResponse,
        advances: Vec<DispatchAdvanceResponse>,
    ) -> Self {
        Self {
            kernel: FakeKernel::new(authorize, advances),
            spawner: Arc::new(FakeSpawner::default()),
            responses: Arc::new(FakeResponses::default()),
            daemon: Arc::new(FakeDaemonJobs::default()),
        }
    }

    pub(crate) fn proxy(&self) -> KernelDispatchProxy {
        KernelDispatchProxy::new(
            self.kernel.clone(),
            Arc::new(MapResolver),
            DispatchSeams {
                spawner: self.spawner.clone(),
                responses: self.responses.clone(),
                daemon_jobs: self.daemon.clone(),
                clock: clock(),
            },
        )
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

pub(crate) fn clock() -> crate::Clock {
    Arc::new(|| "2026-06-13T00:00:00Z".to_string())
}

pub(crate) fn plan(run_id: &str, task_id: &str, agent: &str, deps: &[&str]) -> TaskPlan {
    TaskPlan {
        run_id: run_id.into(),
        task_id: task_id.into(),
        agent: agent.into(),
        command: "claude".into(),
        args: vec![],
        provider: Some("claude".into()),
        runtime: None,
        body: format!("do {task_id}"),
        depends_on: deps.iter().map(|s| s.to_string()).collect(),
        timeout_secs: None,
    }
}

pub(crate) fn done_event(job_id: u32, agent: &str, path: PathBuf) -> FinalResponseEvent {
    FinalResponseEvent {
        job_id,
        agent: agent.into(),
        session_id: None,
        response_path: Some(path),
        response_sha256: Some("deadbeef".into()),
        response_bytes: 12,
        response_preview: "ok".into(),
    }
}

/// A final-response that carries no extracted output — the `Stop` hook fired but
/// nothing was captured. `absorb` turns this into a `Failed` issue.
pub(crate) fn failed_event(job_id: u32, agent: &str, preview: &str) -> FinalResponseEvent {
    FinalResponseEvent {
        job_id,
        agent: agent.into(),
        session_id: None,
        response_path: None,
        response_sha256: None,
        response_bytes: 0,
        response_preview: preview.into(),
    }
}

/// Write a captured-response file the way the daemon's `Stop` hook would, and
/// return its path for a [`done_event`]/`set_latest`.
pub(crate) fn write_response(dir: &Path, name: &str, content: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    path
}

/// A whole dispatch request over `rfc_dir`, with the run knobs the tests don't
/// vary held constant.
pub(crate) fn request(rfc_dir: &Path, tasks: Vec<DispatchTaskSpec>) -> DispatchRequest {
    DispatchRequest {
        rfc_id: "RFC-001".into(),
        project: "demo".into(),
        rfc_dir: rfc_dir.to_path_buf(),
        working_dir: None,
        strategy: "dag".into(),
        max_inflight: 4,
        on_task_fail: "pause".into(),
        tasks,
    }
}

pub(crate) fn task(id: &str, agent: &str, deps: &[&str]) -> DispatchTaskSpec {
    DispatchTaskSpec {
        id: id.into(),
        agent: agent.into(),
        body: format!("do {id}"),
        depends_on: deps.iter().map(|s| s.to_string()).collect(),
    }
}

/// Poll until `f` is true or the deadline elapses. Returns whether it passed.
pub(crate) fn wait_for(mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    f()
}

pub(crate) fn read_issue(rfc_dir: &Path, id: &str) -> Option<Issue> {
    IssueStore::new(rfc_dir).read(id).unwrap()
}

/// `Some(reason)` once the run's `.run.toml` has been closed.
pub(crate) fn run_closed(rfc_dir: &Path) -> Option<String> {
    IssueStore::new(rfc_dir)
        .read_run()
        .unwrap()
        .and_then(|m| m.closed)
}

pub(crate) fn status_is(rfc_dir: &Path, id: &str, want: Status) -> bool {
    read_issue(rfc_dir, id).map(|i| i.meta.status) == Some(want)
}

// ── Resume seeding (pre-restart on-disk state) ───────────────────────

/// Write a live (`closed = None`) or terminal run meta for a resume test.
pub(crate) fn seed_run(rfc_dir: &Path, closed: Option<&str>) {
    IssueStore::new(rfc_dir)
        .write_run(&RunMeta {
            run_id: "r-001".into(),
            plan_hash: "h".into(),
            strategy: "dag".into(),
            started: "2026-06-13T00:00:00Z".into(),
            closed: closed.map(str::to_string),
        })
        .unwrap();
}

/// Write one issue in a chosen state, as a pre-restart run would have left it.
pub(crate) fn seed_issue(
    rfc_dir: &Path,
    id: &str,
    agent: &str,
    deps: &[&str],
    status: Status,
    job_id: Option<u32>,
    response: Option<&str>,
) {
    IssueStore::new(rfc_dir)
        .write(&Issue {
            meta: IssueMeta {
                id: id.into(),
                title: id.into(),
                agent: agent.into(),
                depends_on: deps.iter().map(|s| s.to_string()).collect(),
                status,
                job_id,
                response_sha: None,
                seal: None,
            },
            prompt: format!("do {id}"),
            response: response.map(str::to_string),
        })
        .unwrap();
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Wire contract for the `kernel.v1.dispatch.*` RPCs.
//!
//! `SPEC-ORKIA-RFC-DISPATCH`. Routing one RFC to many agents is the DAG
//! sibling of `@a | @b`: the orchestration **brain** lives in the
//! `orkia-kernel` daemon (Team-gated), while the OSS shell owns PTY task
//! execution. The shell **drives** the run; the kernel **decides**:
//!
//! 1. `authorize` — the shell sends the pre-resolved declarative plan
//!    (tasks + DAG edges + `max_inflight`); the kernel validates the DAG
//!    (acyclic, no dangling deps), authorizes (team/policy), and returns
//!    the first **wave** of ready tasks (up to `max_inflight`).
//! 2. `advance` — after the shell spawns a task and captures its final
//!    response (or sees it fail), it reports the [`TaskOutcome`]; the
//!    kernel folds it into the DAG engine and returns the newly-ready
//!    wave, or a terminal verdict (`Completed` / `Paused` / `Aborted`).
//! 3. `abort` — the shell tells the kernel a run was cancelled so it can
//!    drop the run state.
//!
//! Two deliberate divergences from [`crate::pipeline_kernel`]:
//!
//!   * **Waves, not stages.** A DAG unblocks 0..N tasks per step, so both
//!     responses carry a `Vec<TaskPlan>` rather than one stage.
//!   * **The proxy composes, not the kernel.** A [`TaskPlan`] carries the
//!     raw `body` and `depends_on`; the OSS proxy reads each dependency's
//!     captured final-response file and prepends it before spawning. The
//!     kernel never reads the filesystem, and a real detached job id (not
//!     a synthetic one) keys fan-in on the shell side.
//!
//! As in the pipeline contract, the shell resolves `@agent → command`
//! itself, so the kernel never reads the agent registry. `strategy` and
//! `on_task_fail` cross the wire as documented strings; the kernel maps
//! them onto the brain's enums fail-closed (unknown → refuse).

use serde::{Deserialize, Serialize};

/// JSON-RPC method names for the dispatch RPCs. Single source of truth
/// shared by the kernel server dispatch and the shell-side proxy. (Kept on
/// the module path — not glob-re-exported — so they never collide with the
/// pipeline contract's identically named constants.)
pub const METHOD_AUTHORIZE: &str = "kernel.v1.dispatch.authorize";
pub const METHOD_ADVANCE: &str = "kernel.v1.dispatch.advance";
pub const METHOD_ABORT: &str = "kernel.v1.dispatch.abort";
pub const METHOD_FINALIZE: &str = "kernel.v1.dispatch.finalize";

/// One task as the shell presents it to the kernel: the agent name, the
/// instruction body, the runtime the shell resolved, plus the DAG fields
/// (`id` and `depends_on`). Mirror of [`crate::pipeline_kernel::PipelineStageRequest`]
/// with the graph edges added.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchTaskRequest {
    /// Stable task id, unique within the plan, matching `[a-z0-9-]`.
    pub id: String,
    pub agent: String,
    pub body: String,
    pub command: String,
    pub args: Vec<String>,
    /// Provider name for MCP wiring decisions (e.g. "claude", "codex",
    /// "gemini"). `None` (or unknown) fails authorization — unless the
    /// task is native (`runtime` set), which needs no MCP capture.
    pub provider: Option<String>,
    /// `Some(model_ref)` when the agent is `[runtime] type = "native"`.
    /// `#[serde(default)]` keeps the wire additive (old kernel → `None`).
    #[serde(default)]
    pub runtime: Option<String>,
    /// Task ids this task waits on. Empty for a root. Under
    /// `strategy = "parallel"` or `"sequential"` it must be empty — the
    /// kernel derives the edges — and a non-empty value is refused.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

/// Params for [`METHOD_AUTHORIZE`]: the whole declarative plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchAuthorizeRequest {
    /// `"dag"` (explicit `depends_on`), `"parallel"` (no edges), or
    /// `"sequential"` (kernel chains them in order). Unknown → refused.
    pub strategy: String,
    /// Max tasks running at once (the backpressure / scalability lever).
    /// Must be `>= 1`.
    pub max_inflight: usize,
    /// `"pause"` (default — finish in-flight, hold dependents) or
    /// `"abort"` (tear the whole run down on the first failure).
    pub on_task_fail: String,
    pub tasks: Vec<DispatchTaskRequest>,
}

/// A fully resolved instruction for the shell to spawn exactly one task.
/// Unlike a pipeline `StagePlan`, it carries the raw `body` and the
/// `depends_on` ids: the OSS proxy reads each dependency's captured
/// final-response file and prepends it (the kernel never touches the FS),
/// and the proxy's real detached job id — not a synthetic one — keys
/// fan-in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPlan {
    /// Ledger-derived run id (`r-001`, …), stable for the whole run.
    pub run_id: String,
    pub task_id: String,
    pub agent: String,
    pub command: String,
    pub args: Vec<String>,
    pub provider: Option<String>,
    #[serde(default)]
    pub runtime: Option<String>,
    /// The task's own instruction. The proxy prepends the resolved
    /// dependency outputs before typing it into the agent.
    pub body: String,
    /// Dependencies whose captured output the proxy injects ahead of
    /// `body`. Empty for a root task.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Per-task timeout. `None` → the executor's default.
    pub timeout_secs: Option<u64>,
}

/// Result of [`METHOD_AUTHORIZE`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DispatchAuthorizeResponse {
    /// Run accepted; spawn this first wave (always ≤ `max_inflight`, the
    /// roots of the DAG).
    Authorized {
        run_id: String,
        total_tasks: u32,
        wave: Vec<TaskPlan>,
    },
    /// Rejected before launch (invalid plan, unknown agent/provider, no
    /// team membership, policy denial).
    Refused { reason: String },
}

/// What the shell reports for a finished task: a path to the captured
/// final response plus provenance/timing. A file ref rather than inline
/// bytes keeps the JSON-RPC line small. Mirror of
/// [`crate::pipeline_kernel::StageOutputRef`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskOutputRef {
    /// The file the capturing channel itself wrote (cap 8 MiB): the
    /// final-response `response_path` (primary) or the MCP safety net.
    pub path: String,
    pub bytes: u64,
    /// `true` when captured via the MCP pipe server (safety net), `false`
    /// via the `Stop`-hook final-response channel (primary).
    pub via_mcp: bool,
    pub elapsed_ms: u64,
}

/// The outcome the shell reports for one task in [`METHOD_ADVANCE`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum TaskOutcome {
    /// The task reached its final response.
    Done { output: TaskOutputRef },
    /// The task failed (timeout, non-zero exit, no response, lost on
    /// restart). The kernel applies `on_task_fail`.
    Failed { reason: String },
}

/// Params for [`METHOD_ADVANCE`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchAdvanceRequest {
    pub run_id: String,
    pub task_id: String,
    pub outcome: TaskOutcome,
}

/// Result of [`METHOD_ADVANCE`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DispatchAdvanceResponse {
    /// Spawn these newly-ready tasks. **May be empty** when the reported
    /// task unblocked nothing and others are still in flight — the shell
    /// simply keeps waiting for the rest.
    NextWave { wave: Vec<TaskPlan> },
    /// Every task reached `Done`.
    Completed { elapsed_ms: u64 },
    /// No further progress is possible under `on_task_fail = pause`: the
    /// listed tasks failed and blocked their dependents.
    Paused { failed: Vec<String> },
    /// A task failed under `on_task_fail = abort`; tear the run down.
    Aborted { failed: String },
    /// The kernel could not fold the report in (unknown / already-closed
    /// `run_id`). Fail-closed: the shell stops driving this run.
    Failed { reason: String },
}

/// Params for [`METHOD_ABORT`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchAbortRequest {
    pub run_id: String,
}

/// Result of [`METHOD_ABORT`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchAbortResponse {
    pub ok: bool,
}

/// Params for [`METHOD_FINALIZE`] (SPEC-FLEET-CONVERGENCE-V2, increment 3): the
/// shell reports the RFC-level integration verdict it computed once the DAG
/// drained, and asks the brain to converge the run or re-plan it. The accept
/// command runs shell-side (the brain stays filesystem-free); only its boolean
/// outcome crosses the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchFinalizeRequest {
    pub run_id: String,
    /// Whether the integration `[dispatch].accept` passed this round.
    pub passed: bool,
    /// The fleet round being finalized (`0` = first integration pass).
    pub round: u32,
}

/// Result of [`METHOD_FINALIZE`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DispatchFinalizeResponse {
    /// Integration passed: the run is converged and the kernel dropped it.
    Converged,
    /// Integration failed: re-run this TARGETED wave (the brain re-opened a
    /// subgraph). The shell drives it like any other wave; on the next drain it
    /// finalizes again.
    Replan { wave: Vec<TaskPlan> },
    /// The brain declines to re-plan (nothing actionable to re-open): stop.
    GiveUp { reason: String },
    /// Unknown / already-closed `run_id`. Fail-closed: the shell falls back to
    /// its own re-plan path.
    Failed { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_plan() -> TaskPlan {
        TaskPlan {
            run_id: "r-001".into(),
            task_id: "t-impl".into(),
            agent: "faye".into(),
            command: "claude".into(),
            args: vec!["--mcp-config".into(), "x.json".into()],
            provider: Some("claude".into()),
            runtime: None,
            body: "implement the parser".into(),
            depends_on: vec!["t-api".into()],
            timeout_secs: Some(600),
        }
    }

    #[test]
    fn authorize_request_round_trip() {
        let req = DispatchAuthorizeRequest {
            strategy: "dag".into(),
            max_inflight: 4,
            on_task_fail: "pause".into(),
            tasks: vec![
                DispatchTaskRequest {
                    id: "t-api".into(),
                    agent: "faye".into(),
                    body: "design the api".into(),
                    command: "claude".into(),
                    args: vec![],
                    provider: Some("claude".into()),
                    runtime: None,
                    depends_on: vec![],
                },
                DispatchTaskRequest {
                    id: "t-impl".into(),
                    agent: "sage".into(),
                    body: "implement it".into(),
                    command: "claude".into(),
                    args: vec![],
                    provider: Some("claude".into()),
                    runtime: None,
                    depends_on: vec!["t-api".into()],
                },
            ],
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let back: DispatchAuthorizeRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, back);
    }

    #[test]
    fn task_request_additive_fields_default() {
        // A frame from a pre-native / pre-graph shell (no `runtime`, no
        // `depends_on`) must still deserialize as a root native-less task.
        let legacy = r#"{"id":"t-a","agent":"faye","body":"x","command":"claude","args":[],"provider":"claude"}"#;
        let req: DispatchTaskRequest = serde_json::from_str(legacy).expect("legacy frame");
        assert_eq!(req.runtime, None);
        assert_eq!(req.depends_on, Vec::<String>::new());
    }

    #[test]
    fn authorize_response_tags_variants() {
        let ok = DispatchAuthorizeResponse::Authorized {
            run_id: "r-001".into(),
            total_tasks: 3,
            wave: vec![sample_plan()],
        };
        let json = serde_json::to_string(&ok).expect("serialize");
        assert!(json.contains("\"status\":\"authorized\""));
        let back: DispatchAuthorizeResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(ok, back);

        let refused = DispatchAuthorizeResponse::Refused {
            reason: "no team".into(),
        };
        let json = serde_json::to_string(&refused).expect("serialize");
        assert!(json.contains("\"status\":\"refused\""));
    }

    #[test]
    fn advance_outcome_tags_done_and_failed() {
        let done = DispatchAdvanceRequest {
            run_id: "r-001".into(),
            task_id: "t-api".into(),
            outcome: TaskOutcome::Done {
                output: TaskOutputRef {
                    path: "/d/r-001/t-api.md".into(),
                    bytes: 128,
                    via_mcp: false,
                    elapsed_ms: 4200,
                },
            },
        };
        let json = serde_json::to_string(&done).expect("serialize");
        assert!(json.contains("\"outcome\":\"done\""));
        let back: DispatchAdvanceRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(done, back);

        let failed = DispatchAdvanceRequest {
            run_id: "r-001".into(),
            task_id: "t-impl".into(),
            outcome: TaskOutcome::Failed {
                reason: "timeout".into(),
            },
        };
        let json = serde_json::to_string(&failed).expect("serialize");
        assert!(json.contains("\"outcome\":\"failed\""));
        let back: DispatchAdvanceRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(failed, back);
    }

    #[test]
    fn advance_response_round_trip_all_variants() {
        for resp in [
            DispatchAdvanceResponse::NextWave {
                wave: vec![sample_plan()],
            },
            DispatchAdvanceResponse::NextWave { wave: vec![] },
            DispatchAdvanceResponse::Completed { elapsed_ms: 9001 },
            DispatchAdvanceResponse::Paused {
                failed: vec!["t-impl".into()],
            },
            DispatchAdvanceResponse::Aborted {
                failed: "t-impl".into(),
            },
            DispatchAdvanceResponse::Failed {
                reason: "unknown run_id".into(),
            },
        ] {
            let json = serde_json::to_string(&resp).expect("serialize");
            let back: DispatchAdvanceResponse = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(resp, back);
        }
    }

    #[test]
    fn abort_round_trip() {
        let req = DispatchAbortRequest {
            run_id: "r-001".into(),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let back: DispatchAbortRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, back);
        let resp = DispatchAbortResponse { ok: true };
        let json = serde_json::to_string(&resp).expect("serialize");
        let back: DispatchAbortResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(resp, back);
    }
}

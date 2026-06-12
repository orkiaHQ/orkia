// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Wire contract for the `kernel.v1.pipeline.*` RPCs.
//!
//! In the single-shell architecture the `@a | @b` orchestration brain
//! lives in the `orkia-kernel` daemon, while the shell owns PTY stage
//! execution. The shell **drives** the loop; the kernel **decides**:
//!
//! 1. `authorize` — shell sends the pre-resolved stages; kernel
//!    validates (MCP-capable provider), authorizes (team/policy), and
//!    returns the plan for stage 0.
//! 2. `advance` — after the shell runs a stage and captures its output,
//!    it reports the output back; the kernel composes/enriches the next
//!    stage or declares the run complete.
//! 3. `abort` — the shell tells the kernel a run was cancelled
//!    (Ctrl-C / job kill) so it can drop the run state.
//!
//! The shell resolves `@agent → command` itself (the same resolution it
//! does for Solo dispatch) and sends the runtime in [`PipelineStageRequest`];
//! the kernel never reads the agent registry. The kernel returns a fully
//! resolved [`StagePlan`]; the shell fills run-dir / mcp-config / socket
//! from its *own* config, so the kernel never sees the shell's filesystem
//! or socket layout. This keeps the kernel a pure decision engine that
//! never touches a PTY.

use serde::{Deserialize, Serialize};

/// JSON-RPC method names for the pipeline RPCs. Single source of truth
/// shared by the kernel server dispatch and the shell-side proxy.
pub const METHOD_AUTHORIZE: &str = "kernel.v1.pipeline.authorize";
pub const METHOD_ADVANCE: &str = "kernel.v1.pipeline.advance";
pub const METHOD_ABORT: &str = "kernel.v1.pipeline.abort";

/// One stage as the shell presents it to the kernel: the agent name, the
/// instruction body, and the runtime the shell resolved for it. The
/// kernel validates that `provider` is MCP-capable but does not resolve
/// the command itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineStageRequest {
    pub agent: String,
    pub body: String,
    pub command: String,
    pub args: Vec<String>,
    /// Provider name for MCP wiring decisions (e.g. "claude", "codex",
    /// "gemini"). `None` (or unknown) fails authorization — unless the
    /// stage is native (`runtime` set), which needs no MCP capture.
    pub provider: Option<String>,
    /// `Some(model_ref)` when the agent is `[runtime] type = "native"`
    /// (e.g. `"kimi:k2"`): the shell runs the stage as an Orkia-owned
    /// LLM loop via `kernel.v1.llm.complete` instead of a PTY agent.
    /// `#[serde(default)]` keeps the wire additive: an old kernel sees
    /// `provider: None` and refuses — fail-closed, never mis-executed.
    #[serde(default)]
    pub runtime: Option<String>,
}

/// Params for [`METHOD_AUTHORIZE`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineAuthorizeRequest {
    pub stages: Vec<PipelineStageRequest>,
    /// Bytes from a leading shell pipeline (`<shell> | @a`), prepended to
    /// stage 0's input. `None` for a pure `@a | @b` chain.
    pub shell_prefix: Option<String>,
}

/// A fully resolved instruction for the shell to execute exactly one
/// stage. Everything the executor needs is here; the executor never
/// consults the resolver or the reasoning graph.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagePlan {
    pub pipeline_id: String,
    pub stage_index: u32,
    /// Synthetic, deterministic id keying the MCP envelope and the
    /// final-response fallback lookup. Kernel-assigned so both sides agree.
    pub job_id: u32,
    pub agent: String,
    pub command: String,
    pub args: Vec<String>,
    pub provider: Option<String>,
    /// Copied from [`PipelineStageRequest::runtime`]: `Some(model_ref)`
    /// routes the executor to the native (non-PTY) stage path.
    #[serde(default)]
    pub runtime: Option<String>,
    /// The exact bytes to type into the agent's input box: the
    /// (reasoning-enriched) body composed with the prior stage's carry.
    pub composed_body: String,
    /// Per-stage timeout. `None` → the executor's default.
    pub timeout_secs: Option<u64>,
}

/// Result of [`METHOD_AUTHORIZE`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PipelineAuthorizeResponse {
    /// Run accepted; execute `stage` (always stage 0).
    Authorized {
        pipeline_id: String,
        total_stages: u32,
        stage: StagePlan,
    },
    /// Rejected before launch (unknown agent, non-MCP provider, <2
    /// stages, no team membership, policy denial).
    Refused { reason: String },
}

/// What the shell reports after running a stage: a path to the captured
/// output plus provenance/timing. A file ref rather than inline bytes keeps
/// the JSON-RPC line small.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageOutputRef {
    /// The file the capturing channel itself wrote (cap 8 MiB): the
    /// final-response `response_path` (primary), the MCP `pipeline-output.md`
    /// (safety net), or the transcript fallback — never an intermediate copy.
    pub path: String,
    pub bytes: u64,
    /// `true` when captured via the MCP pipe server (safety net), `false`
    /// via the `Stop`-hook final-response channel (primary).
    pub via_mcp: bool,
    pub elapsed_ms: u64,
}

/// Params for [`METHOD_ADVANCE`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineAdvanceRequest {
    pub pipeline_id: String,
    pub stage_index: u32,
    pub output: StageOutputRef,
}

/// Result of [`METHOD_ADVANCE`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PipelineAdvanceResponse {
    /// Run the next stage.
    NextStage { stage: StagePlan },
    /// All stages done.
    Completed { elapsed_ms: u64 },
    /// The run failed (e.g. the kernel could not read the reported
    /// output). Subsequent stages do not run.
    Failed { stage_index: u32, reason: String },
}

/// Params for [`METHOD_ABORT`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineAbortRequest {
    pub pipeline_id: String,
}

/// Result of [`METHOD_ABORT`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineAbortResponse {
    pub ok: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_plan() -> StagePlan {
        StagePlan {
            pipeline_id: "pid-1".into(),
            stage_index: 0,
            job_id: 42,
            agent: "faye".into(),
            command: "claude".into(),
            args: vec!["--mcp-config".into(), "x.json".into()],
            provider: Some("claude".into()),
            runtime: None,
            composed_body: "summarize\n\ncarry".into(),
            timeout_secs: Some(600),
        }
    }

    #[test]
    fn authorize_request_round_trip() {
        let req = PipelineAuthorizeRequest {
            stages: vec![PipelineStageRequest {
                agent: "faye".into(),
                body: "summarize".into(),
                command: "claude".into(),
                args: vec![],
                provider: Some("claude".into()),
                runtime: None,
            }],
            shell_prefix: None,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let back: PipelineAuthorizeRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, back);
    }

    #[test]
    fn runtime_field_is_additive_on_the_wire() {
        // A frame from a pre-native shell (no `runtime` key) must still
        // deserialize; a native stage round-trips the model ref.
        let legacy =
            r#"{"agent":"faye","body":"x","command":"claude","args":[],"provider":"claude"}"#;
        let req: PipelineStageRequest = serde_json::from_str(legacy).expect("legacy frame");
        assert_eq!(req.runtime, None);

        let native = PipelineStageRequest {
            agent: "kimi".into(),
            body: "x".into(),
            command: String::new(),
            args: vec![],
            provider: None,
            runtime: Some("kimi:k2".into()),
        };
        let json = serde_json::to_string(&native).expect("serialize");
        let back: PipelineStageRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.runtime.as_deref(), Some("kimi:k2"));
    }

    #[test]
    fn authorize_response_tags_variants() {
        let ok = PipelineAuthorizeResponse::Authorized {
            pipeline_id: "pid-1".into(),
            total_stages: 2,
            stage: sample_plan(),
        };
        let json = serde_json::to_string(&ok).expect("serialize");
        assert!(json.contains("\"status\":\"authorized\""));
        let back: PipelineAuthorizeResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(ok, back);

        let refused = PipelineAuthorizeResponse::Refused {
            reason: "no team".into(),
        };
        let json = serde_json::to_string(&refused).expect("serialize");
        assert!(json.contains("\"status\":\"refused\""));
    }

    #[test]
    fn advance_round_trip_all_variants() {
        for resp in [
            PipelineAdvanceResponse::NextStage {
                stage: sample_plan(),
            },
            PipelineAdvanceResponse::Completed { elapsed_ms: 1234 },
            PipelineAdvanceResponse::Failed {
                stage_index: 1,
                reason: "unreadable output".into(),
            },
        ] {
            let json = serde_json::to_string(&resp).expect("serialize");
            let back: PipelineAdvanceResponse = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(resp, back);
        }
    }
}

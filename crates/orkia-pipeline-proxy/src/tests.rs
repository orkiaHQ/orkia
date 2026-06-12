// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

//! Unit tests for the pre-launch paths of [`KernelPipelineProxy`]: stage
//! resolution and authorization refusal. The launched path runs a real
//! agent over a PTY and is covered by the J1.5 real-agent test, not here.

use std::sync::Arc;
use std::time::Duration;

use orkia_shell_types::{
    AgentPipelineRequest, AgentPipelineStage, FinalResponseCallback, FinalResponseEvent,
    FinalResponseSource, IntentGuess, KernelRpc, KernelRpcError, KernelVersion,
    PipelineAuthorizeRequest, PipelineAuthorizeResponse, PipelineDispatchOutcome,
};
use orkia_stage_exec::{StageExecConfig, StageExecutor};

use super::*;

/// Kernel stub: every pipeline authorize returns a fixed response, set per
/// test. The classify/version/shutdown surface is unused here.
struct StubKernel {
    authorize: PipelineAuthorizeResponse,
}

impl KernelRpc for StubKernel {
    fn version(&self) -> KernelVersion {
        KernelVersion {
            protocol: 1,
            kernel: "stub".into(),
            min_client: None,
            capabilities: Vec::new(),
        }
    }
    fn classify_with_timeout(
        &self,
        _line: &str,
        _timeout: Duration,
    ) -> Result<IntentGuess, KernelRpcError> {
        Ok(IntentGuess::Command)
    }
    fn shutdown(&self) -> Result<(), KernelRpcError> {
        Ok(())
    }
    fn pipeline_authorize(
        &self,
        _req: PipelineAuthorizeRequest,
    ) -> Result<PipelineAuthorizeResponse, KernelRpcError> {
        Ok(self.authorize.clone())
    }
}

/// Kernel stub that is unreachable: authorize always errors.
struct DownKernel;

impl KernelRpc for DownKernel {
    fn version(&self) -> KernelVersion {
        KernelVersion {
            protocol: 1,
            kernel: "down".into(),
            min_client: None,
            capabilities: Vec::new(),
        }
    }
    fn classify_with_timeout(
        &self,
        _line: &str,
        _timeout: Duration,
    ) -> Result<IntentGuess, KernelRpcError> {
        Ok(IntentGuess::Command)
    }
    fn shutdown(&self) -> Result<(), KernelRpcError> {
        Ok(())
    }
    fn pipeline_authorize(
        &self,
        _req: PipelineAuthorizeRequest,
    ) -> Result<PipelineAuthorizeResponse, KernelRpcError> {
        Err(KernelRpcError::Unavailable("socket missing".into()))
    }
}

struct OneAgent;
impl StageResolver for OneAgent {
    fn resolve(&self, agent: &str) -> Option<ResolvedRuntime> {
        if agent == "missing" {
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

struct NoFinalResponse;
impl FinalResponseSource for NoFinalResponse {
    fn subscribe(&self, _cb: FinalResponseCallback) {}
    fn latest_for_job(&self, _job_id: u32) -> Option<FinalResponseEvent> {
        None
    }
}

struct NoContext;
impl orkia_stage_exec::StageContextProvider for NoContext {
    fn context_for(&self, _agent: &str) -> Option<orkia_shell::agent_context::AgentContext> {
        None
    }
}

fn executor() -> Arc<StageExecutor> {
    Arc::new(StageExecutor::new(StageExecConfig {
        data_dir: "/data".into(),
        socket_path: "/tmp/orkia.sock".into(),
        final_response_source: Arc::new(NoFinalResponse),
        context_provider: Arc::new(NoContext),
        default_stage_timeout: Duration::from_secs(120),
    }))
}

fn proxy(kernel: Arc<dyn KernelRpc>) -> KernelPipelineProxy {
    KernelPipelineProxy::new(kernel, Arc::new(OneAgent), executor())
}

fn chain(agents: &[&str]) -> AgentPipelineRequest {
    AgentPipelineRequest::AgentChain {
        stages: agents
            .iter()
            .map(|a| AgentPipelineStage {
                agent: (*a).to_string(),
                body: "do".into(),
            })
            .collect(),
    }
}

#[tokio::test]
async fn unresolvable_agent_refused_before_kernel() {
    let kernel = Arc::new(StubKernel {
        authorize: PipelineAuthorizeResponse::Refused {
            reason: "should not be reached".into(),
        },
    });
    let out = proxy(kernel)
        .dispatch_inner(chain(&["faye", "missing"]))
        .await;
    match out {
        PipelineDispatchOutcome::Refused { reason } => assert!(reason.contains("@missing")),
        other => panic!("expected Refused, got {other:?}"),
    }
}

#[tokio::test]
async fn kernel_refusal_surfaces() {
    let kernel = Arc::new(StubKernel {
        authorize: PipelineAuthorizeResponse::Refused {
            reason: "requires Orkia Team".into(),
        },
    });
    let out = proxy(kernel).dispatch_inner(chain(&["faye", "sage"])).await;
    match out {
        PipelineDispatchOutcome::Refused { reason } => assert_eq!(reason, "requires Orkia Team"),
        other => panic!("expected Refused, got {other:?}"),
    }
}

#[tokio::test]
async fn unreachable_kernel_fails_closed() {
    let out = proxy(Arc::new(DownKernel))
        .dispatch_inner(chain(&["faye", "sage"]))
        .await;
    match out {
        PipelineDispatchOutcome::Refused { reason } => {
            assert!(reason.contains("kernel unavailable"))
        }
        other => panic!("expected Refused, got {other:?}"),
    }
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! `KernelPipelineProxy` — the OSS side of `@a | @b`.
//!
//! In the single-shell architecture the orchestration brain lives in the
//! `orkia-kernel` daemon; the shell owns PTY stage execution. This proxy
//! is the seam: it implements [`AgentPipelineCoordinator`] (the trait the
//! REPL dispatches to) without holding any premium logic.
//!
//! Per `dispatch`:
//! 1. Resolve every `@agent → command/args/provider` itself (the kernel
//!    never reads the agent registry).
//! 2. Ask the kernel to `authorize` the resolved stages. The kernel
//!    validates, applies policy, and returns stage 0's plan or a refusal.
//! 3. On authorization, drive the run on a background task (the REPL
//!    returns to the prompt immediately): run each stage
//!    over a real interactive PTY via [`StageExecutor`], report the
//!    captured output, and act on the kernel's `advance` decision until
//!    `Completed`/`Failed`.
//!
//! Fail-closed: an unreachable kernel, an unresolvable agent, or a kernel
//! refusal all surface as [`PipelineDispatchOutcome::Refused`] — premium
//! behaviour is never silently assumed.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod run;

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use orkia_shell_types::{
    AgentPipelineCoordinator, AgentPipelineRequest, AgentPipelineStage, KernelRpc,
    PipelineAuthorizeRequest, PipelineAuthorizeResponse, PipelineDispatchOutcome,
    PipelineProgressCallback, PipelineProgressEvent, PipelineStageRequest,
};
use orkia_stage_exec::StageExecutor;

/// The runtime the shell resolved for one agent stage. Mirrors the fields
/// the kernel needs in [`PipelineStageRequest`]; resolution stays OSS-side.
#[derive(Clone, Debug)]
pub struct ResolvedRuntime {
    pub command: String,
    pub args: Vec<String>,
    pub provider: Option<String>,
    /// `Some(model_ref)` when the agent is `[runtime] type = "native"`:
    /// the stage runs as the Orkia-owned LLM loop instead of a PTY agent.
    pub runtime: Option<String>,
}

/// Maps an agent name to its runtime. Implemented in `bins/orkia` over the
/// agent directory, kept behind a trait so this crate stays free of the
/// agent-loading machinery.
pub trait StageResolver: Send + Sync {
    fn resolve(&self, agent: &str) -> Option<ResolvedRuntime>;
}

/// OSS pipeline coordinator. Cheap to construct; cloneable handles inside.
pub struct KernelPipelineProxy {
    kernel: Arc<dyn KernelRpc>,
    resolver: Arc<dyn StageResolver>,
    executor: Arc<StageExecutor>,
    progress_subs: Arc<Mutex<Vec<PipelineProgressCallback>>>,
}

impl KernelPipelineProxy {
    pub fn new(
        kernel: Arc<dyn KernelRpc>,
        resolver: Arc<dyn StageResolver>,
        executor: Arc<StageExecutor>,
    ) -> Self {
        Self {
            kernel,
            resolver,
            executor,
            progress_subs: Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn dispatch_inner(&self, request: AgentPipelineRequest) -> PipelineDispatchOutcome {
        let stages = match resolve_stages(request.stages(), &*self.resolver) {
            Ok(s) => s,
            Err(reason) => return PipelineDispatchOutcome::Refused { reason },
        };
        let req = PipelineAuthorizeRequest {
            stages,
            shell_prefix: request.shell_prefix().map(str::to_string),
        };
        let auth = match run::authorize(Arc::clone(&self.kernel), req).await {
            Ok(a) => a,
            Err(e) => {
                return PipelineDispatchOutcome::Refused {
                    reason: format!("kernel unavailable: {e}"),
                };
            }
        };
        let (pipeline_id, total_stages, stage) = match auth {
            PipelineAuthorizeResponse::Authorized {
                pipeline_id,
                total_stages,
                stage,
            } => (pipeline_id, total_stages, stage),
            PipelineAuthorizeResponse::Refused { reason } => {
                return PipelineDispatchOutcome::Refused { reason };
            }
        };
        emit_to(
            &self.progress_subs,
            PipelineProgressEvent::Started {
                pipeline_id: pipeline_id.clone(),
                total_stages,
            },
        );
        tokio::spawn(run::drive(self.run_driver(pipeline_id.clone()), stage));
        PipelineDispatchOutcome::Launched {
            pipeline_id,
            total_stages,
        }
    }

    fn run_driver(&self, pipeline_id: String) -> run::RunDriver {
        run::RunDriver {
            kernel: Arc::clone(&self.kernel),
            executor: Arc::clone(&self.executor),
            progress_subs: Arc::clone(&self.progress_subs),
            pipeline_id,
        }
    }
}

impl AgentPipelineCoordinator for KernelPipelineProxy {
    fn dispatch<'a>(
        &'a self,
        request: AgentPipelineRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = PipelineDispatchOutcome> + Send + 'a>> {
        Box::pin(async move { self.dispatch_inner(request).await })
    }

    fn subscribe_progress(&self, callback: PipelineProgressCallback) {
        if let Ok(mut subs) = self.progress_subs.lock() {
            subs.push(callback);
        }
    }
}

/// Resolve every stage to a [`PipelineStageRequest`]. Fails closed on the
/// first agent the shell can't resolve — before the kernel is contacted.
fn resolve_stages(
    stages: &[AgentPipelineStage],
    resolver: &dyn StageResolver,
) -> Result<Vec<PipelineStageRequest>, String> {
    stages.iter().map(|s| resolve_one(s, resolver)).collect()
}

fn resolve_one(
    stage: &AgentPipelineStage,
    resolver: &dyn StageResolver,
) -> Result<PipelineStageRequest, String> {
    let rt = resolver.resolve(&stage.agent).ok_or_else(|| {
        format!(
            "agent @{} has no command configured (pipelines need MCP-capable providers)",
            stage.agent
        )
    })?;
    Ok(PipelineStageRequest {
        agent: stage.agent.clone(),
        body: stage.body.clone(),
        command: rt.command,
        args: rt.args,
        provider: rt.provider,
        runtime: rt.runtime,
    })
}

/// Fan a progress event out to every registered subscriber. Shared with
/// [`run`]; subscribers must not block (they run on the driver task).
pub(crate) fn emit_to(subs: &Mutex<Vec<PipelineProgressCallback>>, event: PipelineProgressEvent) {
    let cbs = match subs.lock() {
        Ok(g) => g.clone(),
        Err(_) => return,
    };
    for cb in cbs {
        cb(event.clone());
    }
}

#[cfg(test)]
mod tests;

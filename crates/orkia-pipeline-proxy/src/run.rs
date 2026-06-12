// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Background drive loop for one authorized pipeline run.
//!
//! The kernel client speaks a blocking std socket, so every RPC is offloaded
//! to a blocking task ([`blocking`]) to keep the async runtime free. The
//! loop alternates: run a stage over a PTY (async), report its output to the
//! kernel (blocking RPC), act on the decision.

use std::sync::{Arc, Mutex};

use orkia_shell_types::{
    KernelRpc, KernelRpcError, PipelineAbortRequest, PipelineAdvanceRequest,
    PipelineAdvanceResponse, PipelineAuthorizeRequest, PipelineAuthorizeResponse,
    PipelineProgressCallback, PipelineProgressEvent, StageOutputRef, StagePlan,
};
use orkia_stage_exec::{StageExecutor, StageOutput};

use crate::emit_to;

/// Everything the background task needs to drive one run to completion.
pub(crate) struct RunDriver {
    pub kernel: Arc<dyn KernelRpc>,
    pub executor: Arc<StageExecutor>,
    pub progress_subs: Arc<Mutex<Vec<PipelineProgressCallback>>>,
    pub pipeline_id: String,
}

/// Outcome of reporting a stage to the kernel.
enum Flow {
    /// Run the returned plan next.
    Next(StagePlan),
    /// The run is over (completed or failed); a terminal event was emitted.
    Done,
}

/// Run `kernel.v1.pipeline.authorize` off the async runtime.
pub(crate) async fn authorize(
    kernel: Arc<dyn KernelRpc>,
    req: PipelineAuthorizeRequest,
) -> Result<PipelineAuthorizeResponse, KernelRpcError> {
    blocking(move || kernel.pipeline_authorize(req)).await
}

/// Drive a run starting from stage 0's plan: spawn each stage, report its
/// output, act on the kernel's decision. Emits progress on every transition.
pub(crate) async fn drive(driver: RunDriver, mut plan: StagePlan) {
    loop {
        emit_spawned(&driver, &plan);
        // Exactly two runtimes: a plan carrying a native
        // model ref runs the Orkia-owned loop, everything else a PTY.
        let result = if plan.runtime.is_some() {
            driver
                .executor
                .run_native_stage(&plan, &driver.kernel)
                .await
        } else {
            driver.executor.run_stage(&plan).await
        };
        let out = match result {
            Ok(o) => o,
            Err(reason) => return fail(&driver, plan.stage_index, reason).await,
        };
        emit(&driver, stage_completed(&driver.pipeline_id, &plan, &out));
        match step(&driver, plan.stage_index, &out).await {
            Flow::Next(next) => plan = next,
            Flow::Done => return,
        }
    }
}

/// Report one stage's output and translate the kernel's reply into a [`Flow`].
async fn step(driver: &RunDriver, stage_index: u32, out: &StageOutput) -> Flow {
    let req = advance_request(&driver.pipeline_id, stage_index, out);
    match blocking_advance(Arc::clone(&driver.kernel), req).await {
        Ok(PipelineAdvanceResponse::NextStage { stage }) => Flow::Next(stage),
        Ok(PipelineAdvanceResponse::Completed { elapsed_ms }) => {
            emit(
                driver,
                PipelineProgressEvent::Completed {
                    pipeline_id: driver.pipeline_id.clone(),
                    elapsed_ms: elapsed_ms as u128,
                },
            );
            Flow::Done
        }
        Ok(PipelineAdvanceResponse::Failed {
            stage_index,
            reason,
        }) => {
            emit(
                driver,
                PipelineProgressEvent::Failed {
                    pipeline_id: driver.pipeline_id.clone(),
                    stage_index,
                    reason,
                },
            );
            Flow::Done
        }
        Err(e) => {
            fail(driver, stage_index, format!("kernel advance failed: {e}")).await;
            Flow::Done
        }
    }
}

/// On any stage failure: best-effort tell the kernel to drop the run, then
/// emit a terminal `Failed` event.
async fn fail(driver: &RunDriver, stage_index: u32, reason: String) {
    let kernel = Arc::clone(&driver.kernel);
    let pipeline_id = driver.pipeline_id.clone();
    let _ = blocking(move || kernel.pipeline_abort(PipelineAbortRequest { pipeline_id })).await;
    emit(
        driver,
        PipelineProgressEvent::Failed {
            pipeline_id: driver.pipeline_id.clone(),
            stage_index,
            reason,
        },
    );
}

async fn blocking_advance(
    kernel: Arc<dyn KernelRpc>,
    req: PipelineAdvanceRequest,
) -> Result<PipelineAdvanceResponse, KernelRpcError> {
    blocking(move || kernel.pipeline_advance(req)).await
}

/// Run a blocking kernel RPC on the blocking pool, mapping a join failure
/// into a transport error.
async fn blocking<T, F>(f: F) -> Result<T, KernelRpcError>
where
    F: FnOnce() -> Result<T, KernelRpcError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r,
        Err(e) => Err(KernelRpcError::Io(format!("kernel rpc task: {e}"))),
    }
}

fn emit(driver: &RunDriver, event: PipelineProgressEvent) {
    emit_to(&driver.progress_subs, event);
}

fn emit_spawned(driver: &RunDriver, plan: &StagePlan) {
    emit(
        driver,
        PipelineProgressEvent::StageSpawned {
            pipeline_id: driver.pipeline_id.clone(),
            stage_index: plan.stage_index,
            agent: plan.agent.clone(),
            job_id: plan.job_id,
        },
    );
}

fn stage_completed(
    pipeline_id: &str,
    plan: &StagePlan,
    out: &StageOutput,
) -> PipelineProgressEvent {
    PipelineProgressEvent::StageCompleted {
        pipeline_id: pipeline_id.to_string(),
        stage_index: plan.stage_index,
        agent: plan.agent.clone(),
        bytes: out.bytes.len() as u64,
        via_mcp: out.via_mcp,
        elapsed_ms: out.elapsed_ms as u128,
    }
}

fn advance_request(
    pipeline_id: &str,
    stage_index: u32,
    out: &StageOutput,
) -> PipelineAdvanceRequest {
    PipelineAdvanceRequest {
        pipeline_id: pipeline_id.to_string(),
        stage_index,
        output: StageOutputRef {
            path: out.output_path.to_string_lossy().into_owned(),
            bytes: out.bytes.len() as u64,
            via_mcp: out.via_mcp,
            elapsed_ms: out.elapsed_ms,
        },
    }
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Collect one stage's output. The first of these to fire wins:
//!
//!   * the `Stop`-hook → final-response channel (primary): the same
//!     turn-end capture Solo dispatch uses. The agent stays alive; we
//!     wake on the `FinalResponseEvent` for this stage's job;
//!   * the MCP `PipelineOutput` envelope (safety net): delivered through
//!     the [`crate::StageExecutor`]'s journal hook into this stage's
//!     channel when a cooperative agent calls `submit_pipeline_output`;
//!   * the child exiting — we then recover its `pipeline-output.md` or,
//!     failing that, its final-response transcript (fallback);
//!   * the stage timeout.
//!
//! The child is torn down and the output waiter cleaned up before
//! returning, on every path.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

use orkia_terminal_core::TerminalEngine;

use crate::interactive::StageAgentDriver;
use crate::{StageExecConfig, StageOutput};

/// A captured stage output: the bytes plus the canonical on-disk file the
/// capturing channel already wrote them to. No channel re-copies — the
/// `path` is the channel's own artifact (final-response `response_path`,
/// the MCP `pipeline-output.md`, or the transcript fallback), and it is
/// what fills [`crate::StageOutput::output_path`] → `StageOutputRef::path`.
#[derive(Clone, Debug)]
pub(crate) struct PipelineOutputPayload {
    pub(crate) bytes: Vec<u8>,
    pub(crate) via_mcp: bool,
    pub(crate) path: PathBuf,
}

/// Live state of a spawned stage, threaded into [`await_output`].
pub(crate) struct StageProc {
    pub(crate) engine: TerminalEngine,
    pub(crate) rx: tokio::sync::mpsc::UnboundedReceiver<PipelineOutputPayload>,
    pub(crate) run_dir: PathBuf,
    pub(crate) job_id: u32,
    pub(crate) stage_index: u32,
    pub(crate) agent: String,
    pub(crate) started: Instant,
    /// Resolved per-stage timeout (plan override or executor default).
    pub(crate) timeout: Duration,
}

/// Wait for content from the final-response channel (primary — the
/// `Stop`-hook turn-end capture), the MCP channel (safety net), the
/// child exiting (recover on-disk content), or the timeout. Tears down
/// the engine before returning. Returns the captured [`StageOutput`] or
/// an error string (timeout / no output).
pub(crate) async fn await_output(
    config: &StageExecConfig,
    driver: &StageAgentDriver,
    proc: &mut StageProc,
) -> Result<StageOutput, String> {
    let stage_timeout = proc.timeout;
    let timeout_fut = tokio::time::sleep(stage_timeout);
    tokio::pin!(timeout_fut);

    // Primary capture: subscribe to the final-response channel exactly as
    // Solo dispatch does (`operator_capture::prepare_projection_capture`).
    // The `Stop` hook fires at turn-end while the agent stays alive, so
    // this wakes us without the child ever exiting. Freshness is anchored
    // to now — the response file for this unique job cannot pre-exist.
    let fresh_baseline = SystemTime::now();
    let (fr_tx, mut fr_rx) = tokio::sync::mpsc::unbounded_channel();
    config.final_response_source.subscribe(std::sync::Arc::new(
        move |event: orkia_shell_types::FinalResponseEvent| {
            let _ = fr_tx.send(event);
        },
    ));

    // The interactive engine has no async `wait()`; the reader thread
    // flips this flag the instant the child's PTY hits EOF. Poll it on a
    // short interval alongside the output channels.
    let child_exited = proc.engine.child_exited_handle();
    let mut exit_poll = tokio::time::interval(Duration::from_millis(100));

    let content: Option<PipelineOutputPayload> = loop {
        tokio::select! {
            event = fr_rx.recv() => {
                if let Some(payload) =
                    event.and_then(|e| final_response_payload(&e, proc.job_id, fresh_baseline))
                {
                    break Some(payload);
                }
            }
            envelope = proc.rx.recv() => {
                if let Some(payload) = envelope {
                    break Some(payload);
                }
            }
            _ = exit_poll.tick() => {
                if child_exited.load(Ordering::SeqCst) {
                    break recover_exited_content(config, &proc.run_dir, proc.job_id);
                }
            }
            _ = &mut timeout_fut => {
                driver.teardown(&proc.engine, proc.job_id);
                return Err(format!(
                    "stage {} (@{}) timed out after {:?}",
                    proc.stage_index, proc.agent, stage_timeout
                ));
            }
        }
    };

    // We have content (or the child exited): stop the typist/detector and
    // signal the agent to exit.
    driver.teardown(&proc.engine, proc.job_id);

    let payload = content.ok_or_else(|| {
        format!(
            "stage {} (@{}) ended without producing output",
            proc.stage_index, proc.agent
        )
    })?;

    // The capturing channel already persisted the output (final-response
    // `response_path`, MCP `pipeline-output.md`, or transcript fallback);
    // hand the kernel that file directly — no intermediate `carry.bin`.
    Ok(StageOutput {
        bytes: payload.bytes,
        via_mcp: payload.via_mcp,
        elapsed_ms: proc.started.elapsed().as_millis() as u64,
        output_path: payload.path,
    })
}

/// Match a `FinalResponseEvent` to this stage's job and read its
/// response file as the captured payload. Returns `None` when the event
/// is for a different job, is stale (file mtime predates the spawn), or
/// the file is missing/unreadable — the select loop then keeps waiting.
fn final_response_payload(
    event: &orkia_shell_types::FinalResponseEvent,
    job_id: u32,
    fresh_baseline: SystemTime,
) -> Option<PipelineOutputPayload> {
    if event.job_id != job_id {
        return None;
    }
    let path = event.response_path.as_ref()?;
    let fresh = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .is_ok_and(|modified| modified >= fresh_baseline);
    if !fresh {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    Some(PipelineOutputPayload {
        bytes,
        via_mcp: false,
        path: PathBuf::from(path),
    })
}

/// After a stage's child exits, recover any content it produced: prefer
/// the MCP-written `pipeline-output.md`, then fall back to the
/// final-response transcript for the job. `None` if neither exists.
fn recover_exited_content(
    config: &StageExecConfig,
    run_dir: &Path,
    job_id: u32,
) -> Option<PipelineOutputPayload> {
    let mcp_path = run_dir.join("pipeline-output.md");
    if let Ok(bytes) = std::fs::read(&mcp_path) {
        return Some(PipelineOutputPayload {
            bytes,
            via_mcp: true,
            path: mcp_path,
        });
    }
    if let Some(fr) = config.final_response_source.latest_for_job(job_id)
        && let Some(path) = fr.response_path.as_ref()
        && let Ok(bytes) = std::fs::read(path)
    {
        return Some(PipelineOutputPayload {
            bytes,
            via_mcp: false,
            path: PathBuf::from(path),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_shell_types::FinalResponseEvent;

    fn event(job_id: u32, path: Option<PathBuf>) -> FinalResponseEvent {
        FinalResponseEvent {
            job_id,
            agent: "faye".into(),
            session_id: None,
            response_path: path,
            response_sha256: None,
            response_bytes: 0,
            response_preview: String::new(),
        }
    }

    fn mtime(path: &Path) -> SystemTime {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .expect("mtime")
    }

    #[test]
    fn fresh_event_for_job_reads_response_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("final-response.md");
        std::fs::write(&path, b"PING").expect("write");
        // Baseline strictly before the file mtime → considered fresh.
        let baseline = mtime(&path) - Duration::from_secs(10);
        let payload = final_response_payload(&event(7, Some(path.clone())), 7, baseline)
            .expect("fresh event should yield payload");
        assert_eq!(payload.bytes, b"PING");
        assert!(!payload.via_mcp);
        // The payload points at the channel's own file — no carry copy.
        assert_eq!(payload.path, path);
    }

    #[test]
    fn event_for_other_job_is_ignored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("final-response.md");
        std::fs::write(&path, b"PING").expect("write");
        let baseline = mtime(&path) - Duration::from_secs(10);
        assert!(final_response_payload(&event(99, Some(path)), 7, baseline).is_none());
    }

    #[test]
    fn stale_event_is_ignored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("final-response.md");
        std::fs::write(&path, b"PING").expect("write");
        // Baseline after the file mtime → considered stale (pre-existing).
        let baseline = mtime(&path) + Duration::from_secs(10);
        assert!(final_response_payload(&event(7, Some(path)), 7, baseline).is_none());
    }

    #[test]
    fn event_without_path_is_ignored() {
        let baseline = SystemTime::now();
        assert!(final_response_payload(&event(7, None), 7, baseline).is_none());
    }
}

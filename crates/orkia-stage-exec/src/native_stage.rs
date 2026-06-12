// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Native pipeline stage: one bounded turn of the Orkia-owned LLM loop
//! (see [`orkia_shell::native`]) instead of a PTY agent. The stage
//! reuses the Solo turn machine, tool executor, and audit emitter —
//! one implementation, two hosts.
//!
//! Capture is direct: the turn's final text IS the stage output, so
//! there is no Stop-hook race, no MCP safety net, no waiter. Audit
//! records travel as NDJSON [`JournalEnvelope`] lines over the shell's
//! journal socket (the same wire the MCP pipe server uses); the hub's
//! unknown-hook catch-all lands the policy verdict on the SEAL surface.
//! Fail-closed: an unreachable journal socket refuses the stage
//! before the first model call — never an unaudited turn.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use orkia_shell::job::JobId;
use orkia_shell::native::emit::NativeEmitter;
use orkia_shell::native::tools::build_tool_executor;
use orkia_shell::native::turn::{TurnCtx, run_turn};
use orkia_shell_types::{
    JournalEnvelope, KernelRpc, NativeChatMessage, NativeContentBlock, StagePlan,
};

use crate::{StageExecConfig, StageOutput, dir};

/// File the stage's final text is written to inside the run dir — what
/// fills [`orkia_shell_types::StageOutputRef::path`].
const NATIVE_OUTPUT_FILE: &str = "native-output.md";

/// Run one native stage end to end: run dir, audited tool executor,
/// journal forwarder, one bounded turn, output file.
pub(crate) async fn run(
    config: &StageExecConfig,
    plan: &StagePlan,
    kernel: &Arc<dyn KernelRpc>,
) -> Result<StageOutput, String> {
    let Some(model) = plan.runtime.as_deref() else {
        return Err("native stage: plan carries no runtime model".into());
    };
    let started = Instant::now();
    let run_dir = dir::create_run_dir(config, plan)?;

    // Same identity assembly as a vendor stage — a native `@kimi` is
    // the real kimi. No pipeline addendum: there is no MCP safety net
    // to advertise, the turn's text is captured directly.
    let context = config.context_provider.context_for(&plan.agent);
    let tools = build_tool_executor(
        &config.data_dir,
        &plan.agent,
        Some(run_dir.clone()),
        context.as_ref().is_some_and(|c| c.knowledge_mcp_bridge),
    );
    let journal_tx = connect_journal_forwarder(&config.socket_path).await?;
    let emitter = NativeEmitter::for_stage(JobId(plan.job_id), plan.agent.clone(), journal_tx);

    let mut transcript: Vec<NativeChatMessage> = Vec::new();
    if let Some(assembled) = context.map(|c| c.assembled) {
        transcript.push(NativeChatMessage {
            role: "system".into(),
            content: vec![NativeContentBlock::Text { text: assembled }],
        });
    }
    transcript.push(NativeChatMessage {
        role: "user".into(),
        content: vec![NativeContentBlock::Text {
            text: plan.composed_body.clone(),
        }],
    });

    let ctx = TurnCtx {
        kernel,
        model,
        tools: &tools,
        emitter: &emitter,
    };
    let timeout = plan
        .timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(config.default_stage_timeout);
    let text = match tokio::time::timeout(timeout, run_turn(&ctx, &mut transcript)).await {
        Ok(Ok(text)) => text,
        Ok(Err(e)) => {
            let reason = e.to_string();
            emitter.turn_failure(&reason);
            return Err(format!("native stage turn failed: {reason}"));
        }
        Err(_) => {
            emitter.turn_failure("stage timeout");
            return Err(format!(
                "native stage timed out after {}s",
                timeout.as_secs()
            ));
        }
    };

    let output_path = run_dir.join(NATIVE_OUTPUT_FILE);
    let bytes = text.into_bytes();
    std::fs::write(&output_path, &bytes).map_err(|e| format!("write native output: {e}"))?;
    Ok(StageOutput {
        bytes,
        via_mcp: false,
        elapsed_ms: started.elapsed().as_millis() as u64,
        output_path,
    })
}

/// Connect the journal socket and spawn the forwarder task that writes
/// each envelope as one NDJSON line — the same wire the MCP pipe server
/// speaks, so the hub listener routes them unchanged. A connect failure
/// fails the stage up front; a later write failure kills the task,
/// which closes the channel and makes the emitter's next audit write
/// abort the in-flight tool call.
async fn connect_journal_forwarder(
    socket_path: &PathBuf,
) -> Result<tokio::sync::mpsc::UnboundedSender<JournalEnvelope>, String> {
    let mut stream = tokio::net::UnixStream::connect(socket_path)
        .await
        .map_err(|e| {
            format!(
                "native stage: journal socket {} unreachable ({e}); \
                 refusing an unaudited stage",
                socket_path.display()
            )
        })?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<JournalEnvelope>();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        while let Some(env) = rx.recv().await {
            let Ok(mut line) = serde_json::to_vec(&env) else {
                continue;
            };
            line.push(b'\n');
            if stream.write_all(&line).await.is_err() {
                break;
            }
        }
    });
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StageContextProvider;
    use orkia_shell_types::{
        FinalResponseEvent, FinalResponseSource, IntentGuess, KernelRpcError, KernelVersion,
        NativeCompletionRequest, NativeCompletionResponse, NativeFinish,
    };
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct NoFinalResponse;
    impl FinalResponseSource for NoFinalResponse {
        fn subscribe(&self, _cb: orkia_shell_types::FinalResponseCallback) {}
        fn latest_for_job(&self, _job_id: u32) -> Option<FinalResponseEvent> {
            None
        }
    }

    struct NoContext;
    impl StageContextProvider for NoContext {
        fn context_for(&self, _agent: &str) -> Option<orkia_shell::agent_context::AgentContext> {
            None
        }
    }

    struct ScriptedKernel {
        script: Mutex<VecDeque<NativeCompletionResponse>>,
    }
    impl KernelRpc for ScriptedKernel {
        fn version(&self) -> KernelVersion {
            KernelVersion {
                protocol: 1,
                kernel: "mock".into(),
                min_client: None,
                capabilities: Vec::new(),
            }
        }
        fn classify_with_timeout(
            &self,
            _line: &str,
            _t: Duration,
        ) -> Result<IntentGuess, KernelRpcError> {
            Err(KernelRpcError::Unavailable("mock".into()))
        }
        fn shutdown(&self) -> Result<(), KernelRpcError> {
            Ok(())
        }
        fn llm_complete(
            &self,
            _req: NativeCompletionRequest,
        ) -> Result<NativeCompletionResponse, KernelRpcError> {
            self.script
                .lock()
                .expect("script lock")
                .pop_front()
                .ok_or(KernelRpcError::Unavailable("script empty".into()))
        }
    }

    fn scripted(script: Vec<NativeCompletionResponse>) -> Arc<dyn KernelRpc> {
        Arc::new(ScriptedKernel {
            script: Mutex::new(script.into()),
        })
    }

    fn end_turn(text: &str) -> NativeCompletionResponse {
        NativeCompletionResponse {
            content: vec![NativeContentBlock::Text { text: text.into() }],
            finish: NativeFinish::EndTurn,
            usage: None,
        }
    }

    fn native_plan() -> StagePlan {
        StagePlan {
            pipeline_id: "pipe-n".into(),
            stage_index: 1,
            job_id: 9,
            agent: "kimi".into(),
            command: String::new(),
            args: vec![],
            provider: None,
            runtime: Some("kimi:k2".into()),
            composed_body: "summarize".into(),
            timeout_secs: Some(5),
        }
    }

    /// A listening journal socket collecting NDJSON lines, plus the
    /// config pointing at it.
    fn config_with_socket(
        dir: &std::path::Path,
    ) -> (StageExecConfig, tokio::task::JoinHandle<Vec<String>>) {
        let socket_path = dir.join("orkia.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let listener = tokio::net::UnixListener::from_std(listener).expect("tokio listener");
        let collector = tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let Ok((stream, _)) = listener.accept().await else {
                return Vec::new();
            };
            let mut lines = tokio::io::BufReader::new(stream).lines();
            let mut out = Vec::new();
            while let Ok(Some(line)) = lines.next_line().await {
                out.push(line);
            }
            out
        });
        let config = StageExecConfig {
            data_dir: dir.to_path_buf(),
            socket_path,
            final_response_source: Arc::new(NoFinalResponse),
            context_provider: Arc::new(NoContext),
            default_stage_timeout: Duration::from_secs(5),
        };
        (config, collector)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_stage_captures_final_text_directly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (config, _collector) = config_with_socket(tmp.path());
        let kernel = scripted(vec![end_turn("native answer")]);
        let plan = native_plan();

        let out = run(&config, &plan, &kernel).await.expect("stage runs");
        assert_eq!(out.bytes, b"native answer");
        assert!(!out.via_mcp);
        assert!(out.output_path.ends_with("native-output.md"));
        let on_disk = std::fs::read(&out.output_path).expect("output file");
        assert_eq!(on_disk, b"native answer");
        assert!(
            out.output_path
                .starts_with(tmp.path().join("pipelines").join("pipe-n"))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tool_audit_records_reach_the_journal_socket() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (config, collector) = config_with_socket(tmp.path());
        // One tool call (denied — no policy file exists, deny-all),
        // then the final answer.
        let kernel = scripted(vec![
            NativeCompletionResponse {
                content: vec![NativeContentBlock::ToolCall {
                    id: "tc_1".into(),
                    name: "shell".into(),
                    input: serde_json::json!({"command": "echo hi"}),
                }],
                finish: NativeFinish::ToolUse,
                usage: None,
            },
            end_turn("done"),
        ]);
        let plan = native_plan();
        let out = run(&config, &plan, &kernel).await.expect("stage runs");
        assert_eq!(out.bytes, b"done");

        // Dropping nothing here: `run` returned, the emitter (and its
        // sender) are gone, the forwarder drained and closed the stream.
        let lines = collector.await.expect("collector");
        let events: Vec<String> = lines
            .iter()
            .map(|l| {
                serde_json::from_str::<JournalEnvelope>(l)
                    .expect("envelope parses")
                    .event
                    .unwrap_or_default()
            })
            .collect();
        assert!(events.contains(&"PreToolUse".to_string()), "{events:?}");
        assert!(events.contains(&"cage.verdict".to_string()), "{events:?}");
        assert!(events.contains(&"PostToolUse".to_string()), "{events:?}");
        // No policy.toml in the test data dir → the shell tool denies.
        let verdict = lines
            .iter()
            .find(|l| l.contains("cage.verdict"))
            .expect("verdict line");
        assert!(verdict.contains("deny"), "{verdict}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unreachable_journal_socket_refuses_the_stage() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = StageExecConfig {
            data_dir: tmp.path().to_path_buf(),
            socket_path: tmp.path().join("missing.sock"),
            final_response_source: Arc::new(NoFinalResponse),
            context_provider: Arc::new(NoContext),
            default_stage_timeout: Duration::from_secs(5),
        };
        let kernel = scripted(vec![end_turn("never")]);
        let err = run(&config, &native_plan(), &kernel)
            .await
            .expect_err("refused");
        assert!(err.contains("unaudited"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn plan_without_runtime_is_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (config, _collector) = config_with_socket(tmp.path());
        let kernel = scripted(vec![]);
        let mut plan = native_plan();
        plan.runtime = None;
        let err = run(&config, &plan, &kernel).await.expect_err("no model");
        assert!(err.contains("no runtime model"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stage_timeout_fails_the_stage() {
        struct SlowKernel;
        impl KernelRpc for SlowKernel {
            fn version(&self) -> KernelVersion {
                KernelVersion {
                    protocol: 1,
                    kernel: "slow".into(),
                    min_client: None,
                    capabilities: Vec::new(),
                }
            }
            fn classify_with_timeout(
                &self,
                _line: &str,
                _t: Duration,
            ) -> Result<IntentGuess, KernelRpcError> {
                Err(KernelRpcError::Unavailable("slow".into()))
            }
            fn shutdown(&self) -> Result<(), KernelRpcError> {
                Ok(())
            }
            fn llm_complete(
                &self,
                _req: NativeCompletionRequest,
            ) -> Result<NativeCompletionResponse, KernelRpcError> {
                std::thread::sleep(Duration::from_secs(10));
                Err(KernelRpcError::Unavailable("too late".into()))
            }
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (config, _collector) = config_with_socket(tmp.path());
        let kernel: Arc<dyn KernelRpc> = Arc::new(SlowKernel);
        let mut plan = native_plan();
        plan.timeout_secs = Some(1);
        let err = run(&config, &plan, &kernel).await.expect_err("timeout");
        assert!(err.contains("timed out"), "{err}");
    }
}

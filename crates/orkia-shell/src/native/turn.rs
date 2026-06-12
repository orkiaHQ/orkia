// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! The native turn machine: completion → tool calls → results →
//! completion, until the model ends the turn or a bound trips.
//!
//! One `run_turn` call = one user-visible turn. The kernel call is
//! synchronous NDJSON RPC, so it runs on the blocking pool — the
//! session task (and the REPL beyond it) never blocks on the network.
//! Everything in a completion is hostile model output: unknown
//! tool names and malformed inputs become tool errors, never panics.

use std::sync::Arc;

use orkia_shell_types::{
    KernelRpc, KernelRpcError, NativeChatMessage, NativeCompletionRequest, NativeContentBlock,
    NativeFinish,
};

use super::emit::{EmitError, NativeEmitter};
use super::tools::ToolExecutor;

/// Hard bound on completion rounds inside one turn. A model looping on
/// tool calls is journaled and stopped, fail-closed.
pub(crate) const MAX_TURN_STEPS: usize = 24;

#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error("kernel: {0}")]
    Kernel(KernelRpcError),
    #[error("model error: {0}")]
    Model(String),
    #[error("model hit the output length limit")]
    Length,
    #[error("turn exceeded {MAX_TURN_STEPS} completion steps; stopping")]
    MaxSteps,
    #[error(transparent)]
    Emit(#[from] EmitError),
    #[error("kernel task join: {0}")]
    Join(String),
}

/// Borrowed per-turn context — the session owns all of it.
pub struct TurnCtx<'a> {
    pub kernel: &'a Arc<dyn KernelRpc>,
    pub model: &'a str,
    pub tools: &'a ToolExecutor,
    pub emitter: &'a NativeEmitter,
}

/// Drive one turn to completion. Appends every assistant message and
/// tool-result message to `transcript` (the caller pushed the user
/// message before calling). Returns the assistant's final text.
pub async fn run_turn(
    ctx: &TurnCtx<'_>,
    transcript: &mut Vec<NativeChatMessage>,
) -> Result<String, TurnError> {
    for _ in 0..MAX_TURN_STEPS {
        let response = complete(ctx, transcript).await?;
        transcript.push(NativeChatMessage {
            role: "assistant".into(),
            content: response.content.clone(),
        });
        match response.finish {
            NativeFinish::EndTurn => return Ok(collect_text(&response.content)),
            NativeFinish::Length => return Err(TurnError::Length),
            NativeFinish::Error { message } => return Err(TurnError::Model(message)),
            NativeFinish::ToolUse => {
                let results = execute_tool_calls(ctx, &response.content).await?;
                if results.is_empty() {
                    // `ToolUse` finish with no ToolCall blocks — hostile
                    // or buggy provider output. Treat whatever text
                    // came along as the final answer rather than looping.
                    return Ok(collect_text(&response.content));
                }
                transcript.push(NativeChatMessage {
                    role: "user".into(),
                    content: results,
                });
            }
        }
    }
    Err(TurnError::MaxSteps)
}

/// One kernel completion on the blocking pool. The transcript is
/// cloned into the task — the session keeps ownership (#2).
async fn complete(
    ctx: &TurnCtx<'_>,
    transcript: &[NativeChatMessage],
) -> Result<orkia_shell_types::NativeCompletionResponse, TurnError> {
    let req = NativeCompletionRequest {
        model: ctx.model.to_string(),
        messages: transcript.to_vec(),
        tools: ctx.tools.defs(),
        max_tokens: None,
    };
    let kernel = Arc::clone(ctx.kernel);
    tokio::task::spawn_blocking(move || kernel.llm_complete(req))
        .await
        .map_err(|e| TurnError::Join(e.to_string()))?
        .map_err(TurnError::Kernel)
}

/// Execute every `ToolCall` block in order, fully audited: the
/// `PreToolUse` record is written BEFORE execution and a journal
/// failure aborts the call. Returns the `ToolResult` blocks for
/// the next completion.
async fn execute_tool_calls(
    ctx: &TurnCtx<'_>,
    content: &[NativeContentBlock],
) -> Result<Vec<NativeContentBlock>, TurnError> {
    let mut results = Vec::new();
    for block in content {
        let NativeContentBlock::ToolCall { id, name, input } = block else {
            continue;
        };
        ctx.emitter.pre_tool_use(name, input)?;
        let outcome = ctx.tools.execute(name, input).await;
        if let Some(note) = &outcome.verdict {
            ctx.emitter.cage_verdict(note);
        }
        ctx.emitter.knowledge_access(&outcome.accessed_node_ids)?;
        ctx.emitter.post_tool_use(name, outcome.is_error)?;
        results.push(NativeContentBlock::ToolResult {
            id: id.clone(),
            content: outcome.content,
            is_error: outcome.is_error,
        });
    }
    Ok(results)
}

/// Concatenated text blocks of an assistant message.
pub(crate) fn collect_text(content: &[NativeContentBlock]) -> String {
    let mut out = String::new();
    for block in content {
        if let NativeContentBlock::Text { text } = block {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::JobId;
    use crate::protocol::EventRouter;
    use orkia_shell_types::{
        IntentGuess, JournalEnvelope, KernelVersion, NativeCompletionResponse, NativeUsage, Policy,
    };
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    /// Kernel mock returning scripted completions in order. Only the
    /// two required trait methods plus `llm_complete` are implemented.
    struct MockKernelRpc {
        script: Mutex<VecDeque<Result<NativeCompletionResponse, KernelRpcError>>>,
    }

    impl MockKernelRpc {
        fn scripted(
            script: Vec<Result<NativeCompletionResponse, KernelRpcError>>,
        ) -> Arc<dyn KernelRpc> {
            Arc::new(Self {
                script: Mutex::new(script.into()),
            })
        }
    }

    impl KernelRpc for MockKernelRpc {
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
                .unwrap_or(Err(KernelRpcError::Unavailable("script empty".into())))
        }
    }

    fn end_turn(text: &str) -> Result<NativeCompletionResponse, KernelRpcError> {
        Ok(NativeCompletionResponse {
            content: vec![NativeContentBlock::Text { text: text.into() }],
            finish: NativeFinish::EndTurn,
            usage: Some(NativeUsage {
                input_tokens: 1,
                output_tokens: 1,
            }),
        })
    }

    fn tool_use(name: &str, input: serde_json::Value) -> NativeCompletionResponse {
        NativeCompletionResponse {
            content: vec![NativeContentBlock::ToolCall {
                id: "tc_1".into(),
                name: name.into(),
                input,
            }],
            finish: NativeFinish::ToolUse,
            usage: None,
        }
    }

    fn allowing_policy() -> Policy {
        toml::from_str(
            r#"
            default_verdict = "deny"

            [workspace]
            root = "."

            [[capabilities]]
            name = "read-only"
            matches = ["echo *"]
            verdict = "allow"
            "#,
        )
        .expect("test policy parses")
    }

    struct Harness {
        kernel: Arc<dyn KernelRpc>,
        tools: ToolExecutor,
        emitter: NativeEmitter,
        journal_rx: tokio::sync::mpsc::UnboundedReceiver<JournalEnvelope>,
    }

    fn harness(script: Vec<Result<NativeCompletionResponse, KernelRpcError>>) -> Harness {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Harness {
            kernel: MockKernelRpc::scripted(script),
            tools: ToolExecutor::new(Some(allowing_policy()), None, None),
            emitter: NativeEmitter::new(JobId(3), "kimi".into(), tx, EventRouter::new()),
            journal_rx: rx,
        }
    }

    fn user(text: &str) -> NativeChatMessage {
        NativeChatMessage {
            role: "user".into(),
            content: vec![NativeContentBlock::Text { text: text.into() }],
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tool_use_then_end_turn_audits_every_call() {
        let mut h = harness(vec![
            Ok(tool_use("shell", serde_json::json!({"command": "echo hi"}))),
            end_turn("done: hi"),
        ]);
        let ctx = TurnCtx {
            kernel: &h.kernel,
            model: "kimi:k2",
            tools: &h.tools,
            emitter: &h.emitter,
        };
        let mut transcript = vec![user("run echo")];
        let text = run_turn(&ctx, &mut transcript).await.expect("turn");
        assert_eq!(text, "done: hi");
        // user, assistant(tool_call), user(tool_result), assistant(text)
        assert_eq!(transcript.len(), 4);
        let pre = h.journal_rx.try_recv().expect("PreToolUse");
        assert_eq!(pre.event.as_deref(), Some("PreToolUse"));
        let post = h.journal_rx.try_recv().expect("PostToolUse");
        assert_eq!(post.event.as_deref(), Some("PostToolUse"));
        assert_eq!(post.exit_code, Some(0));
        // The tool result carried the echo output back to the model.
        let NativeContentBlock::ToolResult { content, .. } = &transcript[2].content[0] else {
            panic!("expected tool result");
        };
        assert_eq!(content.trim(), "hi");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn denied_tool_call_is_audited_and_fed_back() {
        let mut h = harness(vec![
            Ok(tool_use(
                "shell",
                serde_json::json!({"command": "git push origin main"}),
            )),
            end_turn("understood"),
        ]);
        let ctx = TurnCtx {
            kernel: &h.kernel,
            model: "kimi:k2",
            tools: &h.tools,
            emitter: &h.emitter,
        };
        let mut transcript = vec![user("push it")];
        let text = run_turn(&ctx, &mut transcript).await.expect("turn");
        assert_eq!(text, "understood");
        let pre = h.journal_rx.try_recv().expect("PreToolUse");
        assert_eq!(pre.event.as_deref(), Some("PreToolUse"));
        let post = h.journal_rx.try_recv().expect("PostToolUse");
        assert_eq!(post.exit_code, Some(1), "refusal is an error result");
        let NativeContentBlock::ToolResult {
            content, is_error, ..
        } = &transcript[2].content[0]
        else {
            panic!("expected tool result");
        };
        assert!(is_error);
        assert!(content.contains("denied by policy"), "{content}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runaway_tool_loop_stops_at_max_steps() {
        let script = (0..MAX_TURN_STEPS + 2)
            .map(|_| Ok(tool_use("shell", serde_json::json!({"command": "echo x"}))))
            .collect();
        let h = harness(script);
        let ctx = TurnCtx {
            kernel: &h.kernel,
            model: "kimi:k2",
            tools: &h.tools,
            emitter: &h.emitter,
        };
        let mut transcript = vec![user("loop forever")];
        let err = run_turn(&ctx, &mut transcript).await.expect_err("bounded");
        assert!(matches!(err, TurnError::MaxSteps));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hostile_blocks_never_panic() {
        // unknown tool, malformed input, ToolUse finish with no
        // tool calls — all flow through as errors/text, never a panic.
        let h = harness(vec![
            Ok(tool_use("warp_drive", serde_json::json!({"x": [1, {}]}))),
            Ok(NativeCompletionResponse {
                content: vec![NativeContentBlock::Text {
                    text: "no calls but tool_use".into(),
                }],
                finish: NativeFinish::ToolUse,
                usage: None,
            }),
        ]);
        let ctx = TurnCtx {
            kernel: &h.kernel,
            model: "kimi:k2",
            tools: &h.tools,
            emitter: &h.emitter,
        };
        let mut transcript = vec![user("hostile")];
        let text = run_turn(&ctx, &mut transcript).await.expect("turn");
        assert_eq!(text, "no calls but tool_use");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kernel_error_and_model_error_end_the_turn() {
        let h = harness(vec![Err(KernelRpcError::Unavailable("down".into()))]);
        let ctx = TurnCtx {
            kernel: &h.kernel,
            model: "kimi:k2",
            tools: &h.tools,
            emitter: &h.emitter,
        };
        let err = run_turn(&ctx, &mut vec![user("hi")])
            .await
            .expect_err("kernel down");
        assert!(matches!(err, TurnError::Kernel(_)));

        let h2 = harness(vec![Ok(NativeCompletionResponse {
            content: vec![],
            finish: NativeFinish::Error {
                message: "provider 500".into(),
            },
            usage: None,
        })]);
        let ctx2 = TurnCtx {
            kernel: &h2.kernel,
            model: "kimi:k2",
            tools: &h2.tools,
            emitter: &h2.emitter,
        };
        let err2 = run_turn(&ctx2, &mut vec![user("hi")])
            .await
            .expect_err("model error");
        assert!(matches!(err2, TurnError::Model(_)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closed_journal_aborts_before_execution() {
        // the PreToolUse audit write fails → the command must not run.
        let marker = std::env::temp_dir().join("orkia-native-unaudited-marker");
        let _ = std::fs::remove_file(&marker);
        let cmd = format!("echo touched > {}", marker.display());
        let mut h = harness(vec![Ok(tool_use(
            "shell",
            serde_json::json!({ "command": cmd }),
        ))]);
        h.journal_rx.close();
        let ctx = TurnCtx {
            kernel: &h.kernel,
            model: "kimi:k2",
            tools: &h.tools,
            emitter: &h.emitter,
        };
        let err = run_turn(&ctx, &mut vec![user("try")])
            .await
            .expect_err("audit failure aborts");
        assert!(matches!(err, TurnError::Emit(_)));
        assert!(!marker.exists(), "command ran without an audit record");
    }
}

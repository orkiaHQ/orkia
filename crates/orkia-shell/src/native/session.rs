// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! The native session actor: one tokio task owning the transcript,
//! the tool executor, and the emitter for one agent session (#2 — one
//! owner per resource; the REPL talks to it only via the inbound
//! channel and the exit oneshot).
//!
//! Lifecycle: bodies arrive via `tell`/dispatch as
//! [`NativeSessionMsg::User`] and run one turn each, in order. After
//! every turn the outcome publishes through
//! [`FinalResponseService::publish_native`] — the same surface vendor
//! Stop-hook extraction feeds. `Kill` (or the channel closing) ends
//! the session; the exit oneshot drives `try_exit_code` on the
//! [`crate::job::native_entry::NativeJobEntry`], which feeds the
//! normal `Completed` drain → `SessionEnd` SEAL close.

use std::collections::VecDeque;
use std::sync::Arc;

use orkia_final_response::{FinalResponseService, NativePublishRequest};
use orkia_shell_types::{KernelRpc, NativeChatMessage, NativeContentBlock};
use tokio::sync::{mpsc, oneshot};

use super::NativeSessionMsg;
use super::emit::NativeEmitter;
use super::tools::ToolExecutor;
use super::turn::{TurnCtx, run_turn};
use crate::job::JobId;

pub(crate) struct NativeSessionConfig {
    pub job_id: JobId,
    pub agent_name: String,
    pub model: String,
    pub system_prompt: Option<String>,
    pub initial_body: Option<String>,
    pub kernel: Arc<dyn KernelRpc>,
    pub tools: ToolExecutor,
    pub emitter: NativeEmitter,
    pub final_response: Arc<FinalResponseService>,
    pub inbound: mpsc::UnboundedReceiver<NativeSessionMsg>,
    pub exit_tx: oneshot::Sender<i32>,
}

/// Spawn the session actor on the current tokio runtime.
pub(crate) fn spawn(cfg: NativeSessionConfig) {
    let span = tracing::info_span!(
        "native_session",
        job = cfg.job_id.0,
        agent = %cfg.agent_name,
        model = %cfg.model,
    );
    tokio::spawn(tracing::Instrument::instrument(run(cfg), span));
}

async fn run(mut cfg: NativeSessionConfig) {
    let mut transcript: Vec<NativeChatMessage> = Vec::new();
    if let Some(system) = cfg.system_prompt.take() {
        transcript.push(NativeChatMessage {
            role: "system".into(),
            content: vec![NativeContentBlock::Text { text: system }],
        });
    }
    let mut pending: VecDeque<String> = VecDeque::new();
    if let Some(body) = cfg.initial_body.take() {
        pending.push_back(body);
    }
    loop {
        let body = match pending.pop_front() {
            Some(b) => b,
            None => match cfg.inbound.recv().await {
                Some(NativeSessionMsg::User(b)) => b,
                Some(NativeSessionMsg::Kill) | None => break,
            },
        };
        transcript.push(NativeChatMessage {
            role: "user".into(),
            content: vec![NativeContentBlock::Text { text: body }],
        });
        match drive_turn(&mut cfg, &mut transcript, &mut pending).await {
            std::ops::ControlFlow::Continue(outcome) => publish(&cfg, outcome).await,
            std::ops::ControlFlow::Break(()) => break,
        }
    }
    let _ = cfg.exit_tx.send(0);
}

/// Run one turn while staying responsive to the inbound channel:
/// `User` bodies queue for the next turn; `Kill` (or channel close)
/// drops the in-flight turn (V1 — the kernel call itself is not
/// cancellable, but nothing further executes or publishes).
async fn drive_turn(
    cfg: &mut NativeSessionConfig,
    transcript: &mut Vec<NativeChatMessage>,
    pending: &mut VecDeque<String>,
) -> std::ops::ControlFlow<(), Result<String, String>> {
    let ctx = TurnCtx {
        kernel: &cfg.kernel,
        model: &cfg.model,
        tools: &cfg.tools,
        emitter: &cfg.emitter,
    };
    let turn = run_turn(&ctx, transcript);
    tokio::pin!(turn);
    loop {
        tokio::select! {
            result = &mut turn => {
                return std::ops::ControlFlow::Continue(match result {
                    Ok(text) => Ok(text),
                    Err(e) => {
                        let reason = e.to_string();
                        cfg.emitter.turn_failure(&reason);
                        Err(reason)
                    }
                });
            }
            msg = cfg.inbound.recv() => {
                match msg {
                    Some(NativeSessionMsg::User(b)) => pending.push_back(b),
                    Some(NativeSessionMsg::Kill) | None => {
                        return std::ops::ControlFlow::Break(());
                    }
                }
            }
        }
    }
}

/// Publish the turn outcome through the shared final-response surface.
/// Persisting is file I/O — run it on the blocking pool so the actor
/// (and the runtime worker under it) never blocks (#1).
async fn publish(cfg: &NativeSessionConfig, outcome: Result<String, String>) {
    let service = Arc::clone(&cfg.final_response);
    let req = NativePublishRequest {
        job_id: cfg.job_id.0,
        agent: cfg.agent_name.clone(),
        session_id: Some(format!("native-{}", cfg.job_id.0)),
        outcome,
    };
    if let Err(e) = tokio::task::spawn_blocking(move || service.publish_native(req)).await {
        tracing::warn!(job = cfg.job_id.0, "native publish task join failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::EventRouter;
    use orkia_shell_types::{
        IntentGuess, JournalEnvelope, KernelRpcError, KernelVersion, NativeCompletionRequest,
        NativeCompletionResponse, NativeFinish,
    };
    use std::sync::Mutex;
    use std::time::Duration;

    /// Kernel that answers every completion with `EndTurn(echo of the
    /// last user text)` — enough to exercise the actor loop.
    struct EchoKernel {
        calls: Mutex<u32>,
    }

    impl KernelRpc for EchoKernel {
        fn version(&self) -> KernelVersion {
            KernelVersion {
                protocol: 1,
                kernel: "echo".into(),
                min_client: None,
                capabilities: Vec::new(),
            }
        }
        fn classify_with_timeout(
            &self,
            _line: &str,
            _t: Duration,
        ) -> Result<IntentGuess, KernelRpcError> {
            Err(KernelRpcError::Unavailable("echo".into()))
        }
        fn shutdown(&self) -> Result<(), KernelRpcError> {
            Ok(())
        }
        fn llm_complete(
            &self,
            req: NativeCompletionRequest,
        ) -> Result<NativeCompletionResponse, KernelRpcError> {
            *self.calls.lock().expect("calls") += 1;
            let last = req
                .messages
                .iter()
                .rev()
                .find(|m| m.role == "user")
                .and_then(|m| {
                    m.content.iter().find_map(|b| match b {
                        NativeContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                })
                .unwrap_or_default();
            Ok(NativeCompletionResponse {
                content: vec![NativeContentBlock::Text {
                    text: format!("echo: {last}"),
                }],
                finish: NativeFinish::EndTurn,
                usage: None,
            })
        }
    }

    struct SessionHarness {
        inbound_tx: mpsc::UnboundedSender<NativeSessionMsg>,
        exit_rx: oneshot::Receiver<i32>,
        journal_rx: mpsc::UnboundedReceiver<JournalEnvelope>,
        _dir: tempfile::TempDir,
    }

    fn spawn_session(initial_body: Option<&str>) -> SessionHarness {
        let dir = tempfile::tempdir().expect("tempdir");
        let (journal_tx, journal_rx) = mpsc::unbounded_channel();
        let (inbound_tx, inbound) = mpsc::unbounded_channel();
        let (exit_tx, exit_rx) = oneshot::channel();
        let service =
            FinalResponseService::new(dir.path().to_path_buf(), journal_tx.clone()).into_arc();
        let emitter = NativeEmitter::new(JobId(5), "kimi".into(), journal_tx, EventRouter::new());
        spawn(NativeSessionConfig {
            job_id: JobId(5),
            agent_name: "kimi".into(),
            model: "kimi:k2".into(),
            system_prompt: Some("you are kimi".into()),
            initial_body: initial_body.map(str::to_string),
            kernel: Arc::new(EchoKernel {
                calls: Mutex::new(0),
            }),
            tools: ToolExecutor::new(None, None, None),
            emitter,
            final_response: service,
            inbound,
            exit_tx,
        });
        SessionHarness {
            inbound_tx,
            exit_rx,
            journal_rx,
            _dir: dir,
        }
    }

    async fn next_afr(rx: &mut mpsc::UnboundedReceiver<JournalEnvelope>) -> JournalEnvelope {
        loop {
            let env = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("envelope before timeout")
                .expect("channel open");
            if env.event.as_deref() == Some("AgentFinalResponse") {
                return env;
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn initial_body_runs_a_turn_and_publishes() {
        let mut h = spawn_session(Some("hello"));
        let env = next_afr(&mut h.journal_rx).await;
        assert_eq!(env.job_id, Some(5));
        assert_eq!(env.agent.as_deref(), Some("kimi"));
        let preview = env.response_preview.expect("preview");
        assert!(preview.contains("echo: hello"), "{preview}");
        // Session stays alive for follow-ups; kill ends it with exit 0.
        h.inbound_tx.send(NativeSessionMsg::Kill).expect("send");
        let code = tokio::time::timeout(Duration::from_secs(5), h.exit_rx)
            .await
            .expect("exit before timeout")
            .expect("exit code");
        assert_eq!(code, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tell_drives_follow_up_turns_in_order() {
        let mut h = spawn_session(None);
        h.inbound_tx
            .send(NativeSessionMsg::User("first".into()))
            .expect("send");
        h.inbound_tx
            .send(NativeSessionMsg::User("second".into()))
            .expect("send");
        let env1 = next_afr(&mut h.journal_rx).await;
        assert!(
            env1.response_preview
                .as_deref()
                .is_some_and(|p| p.contains("echo: first"))
        );
        let env2 = next_afr(&mut h.journal_rx).await;
        assert!(
            env2.response_preview
                .as_deref()
                .is_some_and(|p| p.contains("echo: second"))
        );
        drop(h.inbound_tx); // channel close == session end
        let code = tokio::time::timeout(Duration::from_secs(5), h.exit_rx)
            .await
            .expect("exit before timeout")
            .expect("exit code");
        assert_eq!(code, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn turn_failure_publishes_failure_and_keeps_session_alive() {
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
                _t: Duration,
            ) -> Result<IntentGuess, KernelRpcError> {
                Err(KernelRpcError::Unavailable("down".into()))
            }
            fn shutdown(&self) -> Result<(), KernelRpcError> {
                Ok(())
            }
            fn llm_complete(
                &self,
                _req: NativeCompletionRequest,
            ) -> Result<NativeCompletionResponse, KernelRpcError> {
                Err(KernelRpcError::Unavailable("provider down".into()))
            }
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let (journal_tx, mut journal_rx) = mpsc::unbounded_channel();
        let (inbound_tx, inbound) = mpsc::unbounded_channel();
        let (exit_tx, exit_rx) = oneshot::channel();
        let service =
            FinalResponseService::new(dir.path().to_path_buf(), journal_tx.clone()).into_arc();
        spawn(NativeSessionConfig {
            job_id: JobId(6),
            agent_name: "kimi".into(),
            model: "kimi:k2".into(),
            system_prompt: None,
            initial_body: Some("hi".into()),
            kernel: Arc::new(DownKernel),
            tools: ToolExecutor::new(None, None, None),
            emitter: NativeEmitter::new(JobId(6), "kimi".into(), journal_tx, EventRouter::new()),
            final_response: service,
            inbound,
            exit_tx,
        });
        // The failed turn journals a NativeTurnError, then a failure AFR.
        let env = next_afr(&mut journal_rx).await;
        let preview = env.response_preview.expect("preview");
        assert!(preview.starts_with("<extraction failed:"), "{preview}");
        // Still tellable: a Kill afterwards exits cleanly.
        inbound_tx.send(NativeSessionMsg::Kill).expect("send");
        let code = tokio::time::timeout(Duration::from_secs(5), exit_rx)
            .await
            .expect("exit before timeout")
            .expect("exit code");
        assert_eq!(code, 0);
    }
}

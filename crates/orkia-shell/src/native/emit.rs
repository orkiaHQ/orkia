// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Journal + SEAL emission for native sessions.
//!
//! The native runtime has no hook process: the session task itself is
//! the witness. It emits the same envelopes a vendor agent's hooks
//! produce (`PreToolUse`/`PostToolUse` with `source: "native"`) so the
//! existing SEAL routing, journal store, and attention surfaces work
//! unchanged — no new variants anywhere downstream.
//!
//! Fail-closed: the methods used to audit a tool call return
//! `Result` — a closed journal channel aborts the call rather than
//! executing unaudited.

use orkia_shell_types::{EventType, JournalEnvelope};
use tokio::sync::mpsc::UnboundedSender;

use crate::job::JobId;
use crate::protocol::EventRouter;

pub(crate) const NATIVE_SOURCE: &str = "native";

/// Cap on the input summary recorded per tool call. The full command
/// already reaches SEAL via the `cage.verdict` event; this is the
/// journal-readable line.
const SUMMARY_CAP: usize = 200;

#[derive(Debug, thiserror::Error)]
#[error("journal channel closed; refusing unaudited tool call")]
pub struct EmitError;

/// Spawn-time hashes for the SEAL genesis event, mirroring
/// [`crate::job::SpawnResult`] for the PTY path.
pub(crate) struct GenesisHashes {
    pub system_prompt_hash: String,
    pub memory_hash: String,
    pub tools_count: usize,
}

/// One emitter per session, owned by the session task.
pub struct NativeEmitter {
    job_id: JobId,
    agent: String,
    session_id: String,
    journal_tx: UnboundedSender<JournalEnvelope>,
    /// `Some` in-process (REPL/runtime — customs reach the SEAL
    /// consumer via the live router). `None` for a pipeline stage:
    /// the emitter has no live router, so custom events (the policy
    /// verdict) travel as Hook envelopes through `journal_tx` instead
    /// — the hub's catch-all converts unknown hook names to `Custom`,
    /// landing on the same SEAL surface.
    router: Option<EventRouter>,
}

impl NativeEmitter {
    pub(crate) fn new(
        job_id: JobId,
        agent: String,
        journal_tx: UnboundedSender<JournalEnvelope>,
        router: EventRouter,
    ) -> Self {
        let session_id = format!("native-{}", job_id.0);
        Self {
            job_id,
            agent,
            session_id,
            journal_tx,
            router: Some(router),
        }
    }

    /// Emitter for a native pipeline stage: every record — including
    /// the policy verdict — goes through `journal_tx` (forwarded to
    /// the shell's journal socket by the stage executor). No genesis:
    /// stages open no SEAL chain, same as vendor stages.
    pub fn for_stage(
        job_id: JobId,
        agent: String,
        journal_tx: UnboundedSender<JournalEnvelope>,
    ) -> Self {
        let session_id = format!("native-{}", job_id.0);
        Self {
            job_id,
            agent,
            session_id,
            journal_tx,
            router: None,
        }
    }

    fn hook_envelope(&self, event: &str) -> JournalEnvelope {
        let mut env = JournalEnvelope::now(EventType::Hook);
        env.event = Some(event.into());
        env.job_id = Some(self.job_id.0);
        env.session_id = Some(self.session_id.clone());
        env.source = Some(NATIVE_SOURCE.into());
        env.agent = Some(self.agent.clone());
        env
    }

    fn send(&self, env: JournalEnvelope) -> Result<(), EmitError> {
        self.journal_tx.send(env).map_err(|_| EmitError)
    }

    /// SEAL genesis, byte-compatible with
    /// [`crate::job::lifecycle::SealChainLifecycle::on_spawn`]. Skipped
    /// when the agent has no filesystem definition (empty hash) — same
    /// no-chain rule as the PTY path. Sessions only ([`Self::new`]):
    /// a stage emitter has no router and opens no chain.
    pub(crate) fn genesis(&self, hashes: &GenesisHashes) {
        let Some(router) = self.router.as_ref() else {
            return;
        };
        if hashes.system_prompt_hash.is_empty() {
            return;
        }
        router.on_custom(
            self.job_id,
            &self.agent,
            "agent.spawn",
            serde_json::json!({
                "job_id": self.job_id.0,
                "agent": &self.agent,
                "system_prompt_hash": hashes.system_prompt_hash,
                "memory_hash": hashes.memory_hash,
                "tools_count": hashes.tools_count,
            }),
        );
    }

    /// Audit record BEFORE a tool executes. An error here must abort
    /// the call.
    pub fn pre_tool_use(&self, tool: &str, input: &serde_json::Value) -> Result<(), EmitError> {
        let mut env = self.hook_envelope("PreToolUse");
        env.tool = Some(tool.into());
        env.description = Some(summarize_input(input));
        self.send(env)
    }

    /// Audit record after a tool call returns (or is refused).
    pub fn post_tool_use(&self, tool: &str, is_error: bool) -> Result<(), EmitError> {
        let mut env = self.hook_envelope("PostToolUse");
        env.tool = Some(tool.into());
        env.exit_code = Some(i32::from(is_error));
        self.send(env)
    }

    /// Policy-gate verdict naming the rule, sealed via the custom-event
    /// path (the SEAL consumer records it in the job chain). Without a
    /// router (stage mode) the verdict travels as a Hook envelope — the
    /// hub's unknown-hook catch-all converts it to the same `Custom`
    /// payload downstream.
    pub fn cage_verdict(&self, note: &super::tools::VerdictNote) {
        match self.router.as_ref() {
            Some(router) => router.on_custom(
                self.job_id,
                &self.agent,
                "cage.verdict",
                serde_json::json!({
                    "decision": note.decision,
                    "capability": note.capability,
                    "rule": note.rule,
                    "command": note.command,
                }),
            ),
            None => {
                let mut env = self.hook_envelope("cage.verdict");
                env.tool = Some(super::tools::SHELL_TOOL.into());
                env.description = Some(note.command.clone());
                env.message = Some(format!(
                    "{} (rule: {})",
                    note.decision,
                    note.rule.as_deref().unwrap_or("default"),
                ));
                let _ = self.send(env);
            }
        }
    }

    /// nodes the `recall_knowledge` tool served.
    pub fn knowledge_access(&self, node_ids: &[String]) -> Result<(), EmitError> {
        if node_ids.is_empty() {
            return Ok(());
        }
        let mut env = JournalEnvelope::knowledge_access(Some(self.job_id.0), node_ids);
        env.source = Some(NATIVE_SOURCE.into());
        env.agent = Some(self.agent.clone());
        self.send(env)
    }

    /// A turn that ended in failure (kernel error, max-steps, length).
    /// Journaled as a hook envelope so the SEAL catch-all records it;
    /// best-effort — the session is already tearing the turn down.
    pub fn turn_failure(&self, reason: &str) {
        let mut env = self.hook_envelope("NativeTurnError");
        env.message = Some(reason.to_string());
        let _ = self.send(env);
    }
}

/// One-line, size-capped summary of a tool input for the journal.
/// Input is hostile model output — rendered, never interpreted.
fn summarize_input(input: &serde_json::Value) -> String {
    let text = input
        .get("command")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| input.to_string());
    let mut s = text.replace('\n', " ");
    if s.len() > SUMMARY_CAP {
        let mut cut = SUMMARY_CAP;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emitter() -> (
        NativeEmitter,
        tokio::sync::mpsc::UnboundedReceiver<JournalEnvelope>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let router = EventRouter::new();
        (NativeEmitter::new(JobId(7), "kimi".into(), tx, router), rx)
    }

    #[test]
    fn pre_tool_use_envelope_shape() {
        let (em, mut rx) = emitter();
        em.pre_tool_use("shell", &serde_json::json!({"command": "ls -la"}))
            .expect("send");
        let env = rx.try_recv().expect("envelope");
        assert_eq!(env.event_type, EventType::Hook);
        assert_eq!(env.event.as_deref(), Some("PreToolUse"));
        assert_eq!(env.job_id, Some(7));
        assert_eq!(env.source.as_deref(), Some(NATIVE_SOURCE));
        assert_eq!(env.agent.as_deref(), Some("kimi"));
        assert_eq!(env.tool.as_deref(), Some("shell"));
        assert_eq!(env.description.as_deref(), Some("ls -la"));
    }

    #[test]
    fn post_tool_use_maps_error_to_exit_code() {
        let (em, mut rx) = emitter();
        em.post_tool_use("shell", true).expect("send");
        let env = rx.try_recv().expect("envelope");
        assert_eq!(env.event.as_deref(), Some("PostToolUse"));
        assert_eq!(env.exit_code, Some(1));
        em.post_tool_use("shell", false).expect("send");
        assert_eq!(rx.try_recv().expect("env").exit_code, Some(0));
    }

    #[test]
    fn closed_channel_is_an_error_not_a_silent_drop() {
        let (em, rx) = emitter();
        drop(rx);
        assert!(em.pre_tool_use("shell", &serde_json::json!({})).is_err());
        assert!(em.post_tool_use("shell", false).is_err());
    }

    #[test]
    fn summary_is_capped_and_single_line() {
        let long = "x".repeat(500);
        let s = summarize_input(&serde_json::json!({"command": format!("a\nb {long}")}));
        assert!(s.len() <= SUMMARY_CAP + '…'.len_utf8());
        assert!(!s.contains('\n'));
    }

    #[test]
    fn stage_mode_verdict_travels_as_hook_envelope() {
        // Without a router (pipeline stage), the policy verdict must
        // still reach the journal channel — as a Hook envelope the
        // hub's unknown-hook catch-all converts to `Custom`.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let em = NativeEmitter::for_stage(JobId(9), "kimi".into(), tx);
        em.cage_verdict(&super::super::tools::VerdictNote {
            decision: "deny",
            capability: None,
            rule: Some("no-push".into()),
            command: "git push".into(),
        });
        let env = rx.try_recv().expect("envelope");
        assert_eq!(env.event_type, EventType::Hook);
        assert_eq!(env.event.as_deref(), Some("cage.verdict"));
        assert_eq!(env.job_id, Some(9));
        assert_eq!(env.source.as_deref(), Some(NATIVE_SOURCE));
        assert_eq!(env.description.as_deref(), Some("git push"));
        assert_eq!(env.message.as_deref(), Some("deny (rule: no-push)"));
    }

    #[test]
    fn stage_mode_genesis_is_a_noop() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let em = NativeEmitter::for_stage(JobId(9), "kimi".into(), tx);
        em.genesis(&GenesisHashes {
            system_prompt_hash: "abc".into(),
            memory_hash: "def".into(),
            tools_count: 2,
        });
        assert!(rx.try_recv().is_err(), "stage opens no SEAL chain");
    }

    #[test]
    fn knowledge_access_skips_empty() {
        let (em, mut rx) = emitter();
        em.knowledge_access(&[]).expect("ok");
        assert!(rx.try_recv().is_err());
        em.knowledge_access(&["node-1".into()]).expect("send");
        let env = rx.try_recv().expect("envelope");
        assert_eq!(env.knowledge_access_ids(), vec!["node-1".to_string()]);
    }
}

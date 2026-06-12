// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! The hot-path capture task. Subscribes to the journal broadcast bus (the
//! *exact* bus SEAL reads — no new socket, no new owner), filters hook events,
//! stamps project/RFC scope, scrubs + hashes, and writes turns to the local
//! store. Never blocks the REPL.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use tokio::sync::broadcast;
use uuid::Uuid;

use orkia_reasoning_core::dto::{RfcRef, TurnDto};
use orkia_reasoning_core::enums::{TurnKind, TurnRelation, TurnRole};
use orkia_reasoning_store::{ReasoningStore, StoreError, TurnInsert};
use orkia_shell_types::journal::{EventType, JournalEnvelope};

use super::scope::{JobScope, JobScopes, new_job_scopes, scope_for};
use super::scrub::{content_hash, scrub_summary};

/// The capture scope stamped onto every turn: the workspace/account that owns
/// the session, plus the optionally-active project and RFC defaults.
///
/// `project_id`/`rfc_ref` here are session-level fallbacks; the live per-job
/// project/RFC is resolved from the [`JobScopes`] map the REPL maintains.
#[derive(Clone)]
pub struct CaptureScope {
    pub workspace_id: Uuid,
    pub account_id: Uuid,
    pub project_id: Option<Uuid>,
    pub rfc_ref: Option<RfcRef>,
}

/// Per-job capture bookkeeping (single-task owned — no lock).
struct JobTrack {
    session_id: Uuid,
    seq: i64,
    last_turn: Option<Uuid>,
}

/// What a hook event maps to in the graph.
enum EventClass {
    SessionStart,
    SessionEnd,
    Turn {
        role: TurnRole,
        kind: TurnKind,
        raw: String,
    },
}

/// Owns the store and the per-job tracking. One instance, one task.
pub struct ReasoningConsumer {
    store: ReasoningStore,
    scope: CaptureScope,
    job_scopes: JobScopes,
    jobs: HashMap<u32, JobTrack>,
}

impl ReasoningConsumer {
    /// Consumer with no live per-job attribution — every turn falls back to the
    /// session-level `scope`. Used by unit tests.
    pub fn new(store: ReasoningStore, scope: CaptureScope) -> Self {
        Self::with_job_scopes(store, scope, new_job_scopes())
    }

    /// Consumer that resolves each turn's project/RFC from the shared
    /// `job_scopes` map (the REPL is the writer; see [`super::scope`]).
    pub fn with_job_scopes(
        store: ReasoningStore,
        scope: CaptureScope,
        job_scopes: JobScopes,
    ) -> Self {
        Self {
            store,
            scope,
            job_scopes,
            jobs: HashMap::new(),
        }
    }

    /// Resolve the project/RFC for a job: the live per-job scope when set,
    /// otherwise the session-level fallback.
    fn resolve_scope(&self, job_id: u32) -> JobScope {
        let js = scope_for(&self.job_scopes, job_id);
        JobScope {
            project_id: js.project_id.or(self.scope.project_id),
            rfc_ref: js.rfc_ref.or_else(|| self.scope.rfc_ref.clone()),
        }
    }

    /// Drain the bus until it closes. Lagged frames are logged and skipped —
    /// reasoning capture is best-effort and must never stall the bus.
    pub async fn run(mut self, mut rx: broadcast::Receiver<JournalEnvelope>) {
        loop {
            match rx.recv().await {
                Ok(env) => {
                    if let Err(e) = self.ingest(&env) {
                        tracing::error!(error = %e, "reasoning: ingest failed — turn dropped");
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "reasoning: consumer lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        tracing::debug!("reasoning consumer: bus closed, exiting");
    }

    /// Process one envelope. Returns the new turn id when a turn was written.
    /// Public for unit tests (drive without the tokio task).
    pub fn ingest(&mut self, env: &JournalEnvelope) -> Result<Option<Uuid>, StoreError> {
        // A premium agent's KG read (served by `orkia-knowledge-mcp`): apply the
        // access bump here, where the single store writer lives (the MCP handle
        // ids are skipped, never a panic).
        if env.event_type == EventType::KnowledgeAccess {
            self.apply_access(env)?;
            return Ok(None);
        }
        if env.event_type != EventType::Hook {
            return Ok(None);
        }
        let Some(job_id) = env.job_id else {
            return Ok(None);
        };
        let Some(class) = classify_event(env) else {
            return Ok(None);
        };
        match class {
            EventClass::SessionStart => {
                self.open_session(job_id, env)?;
                Ok(None)
            }
            EventClass::SessionEnd => {
                self.close_session(job_id)?;
                Ok(None)
            }
            EventClass::Turn { role, kind, raw } => {
                let id = self.write_turn(job_id, env, role, kind, raw)?;
                Ok(Some(id))
            }
        }
    }

    /// Apply the decay-signal bump for a `KnowledgeAccess` event. The MCP handle
    /// that served the read is strictly read-only; this task is the single
    /// store writer, so the bump lands here. Unparseable ids are skipped, never a
    /// panic.
    fn apply_access(&self, env: &JournalEnvelope) -> Result<(), StoreError> {
        let ids: Vec<Uuid> = env
            .knowledge_access_ids()
            .iter()
            .filter_map(|s| Uuid::parse_str(s).ok())
            .collect();
        if !ids.is_empty() {
            self.store.touch_nodes_accessed(&ids)?;
        }
        Ok(())
    }

    fn open_session(&mut self, job_id: u32, env: &JournalEnvelope) -> Result<(), StoreError> {
        let scope = self.resolve_scope(job_id);
        let session_id = self.store.create_session(&self.new_session(env, &scope))?;
        self.jobs.insert(
            job_id,
            JobTrack {
                session_id,
                seq: 0,
                last_turn: None,
            },
        );
        Ok(())
    }

    fn close_session(&mut self, job_id: u32) -> Result<(), StoreError> {
        if let Some(track) = self.jobs.remove(&job_id) {
            self.store.set_session_status(
                track.session_id,
                orkia_reasoning_core::enums::SessionStatus::Completed,
            )?;
        }
        Ok(())
    }

    fn write_turn(
        &mut self,
        job_id: u32,
        env: &JournalEnvelope,
        role: TurnRole,
        kind: TurnKind,
        raw: String,
    ) -> Result<Uuid, StoreError> {
        // Lazily open a session if we joined mid-stream (no SessionStart seen).
        if !self.jobs.contains_key(&job_id) {
            self.open_session(job_id, env)?;
        }
        let (session_id, seq, parent) = {
            let t = self
                .jobs
                .get_mut(&job_id)
                .ok_or_else(|| StoreError::Corrupt("job track vanished".into()))?;
            t.seq += 1;
            (t.session_id, t.seq, t.last_turn)
        };
        let relation = parent.map(|_| match kind {
            TurnKind::ToolResult(_) => TurnRelation::ToolResult,
            _ => TurnRelation::FollowUp,
        });
        let scope = self.resolve_scope(job_id);
        let dto = self.build_dto(&scope, session_id, env, role, kind, &raw, parent, relation);
        let id = self.store.insert_turn(&TurnInsert {
            dto: &dto,
            seq,
            thinking_trace: None,
            thinking_tokens: None,
        })?;
        self.store.bump_turn_count(session_id)?;
        if let Some(t) = self.jobs.get_mut(&job_id) {
            t.last_turn = Some(id);
        }
        Ok(id)
    }

    #[allow(clippy::too_many_arguments)] // builder-free DTO assembly; all fields required
    fn build_dto(
        &self,
        scope: &JobScope,
        session_id: Uuid,
        env: &JournalEnvelope,
        role: TurnRole,
        kind: TurnKind,
        raw: &str,
        parent: Option<Uuid>,
        relation: Option<TurnRelation>,
    ) -> TurnDto {
        let summary = scrub_summary(raw);
        TurnDto {
            client_event_id: Uuid::new_v4(),
            session_id: Some(session_id),
            workspace_id: self.scope.workspace_id,
            project_id: scope.project_id,
            rfc_ref: scope.rfc_ref.clone(),
            agent_name: env.agent.clone().unwrap_or_else(|| "unknown".into()),
            role,
            kind,
            summary,
            content_hash: content_hash(raw),
            token_count: None,
            metadata: build_metadata(env),
            parent_turn_id: parent,
            relation,
            occurred_at: parse_ts(env.timestamp.as_str()),
        }
    }

    fn new_session(
        &self,
        env: &JournalEnvelope,
        scope: &JobScope,
    ) -> orkia_reasoning_store::NewSession {
        orkia_reasoning_store::NewSession {
            workspace_id: self.scope.workspace_id,
            account_id: self.scope.account_id,
            agent_name: env.agent.clone().unwrap_or_else(|| "unknown".into()),
            project_id: scope.project_id,
            rfc_ref: scope.rfc_ref.clone(),
        }
    }
}

/// Map a hook envelope to its graph class. Unknown hook events → `None`.
fn classify_event(env: &JournalEnvelope) -> Option<EventClass> {
    let event = env.event.as_deref()?;
    match event {
        "SessionStart" => Some(EventClass::SessionStart),
        "SessionEnd" => Some(EventClass::SessionEnd),
        "UserPromptSubmit" => Some(EventClass::Turn {
            role: TurnRole::User,
            kind: TurnKind::UserPrompt,
            raw: env.prompt.clone().unwrap_or_default(),
        }),
        "PreToolUse" => Some(EventClass::Turn {
            role: TurnRole::Tool,
            kind: TurnKind::ToolCall(tool_name(env)),
            raw: best_text(env),
        }),
        "PostToolUse" => Some(EventClass::Turn {
            role: TurnRole::Tool,
            kind: TurnKind::ToolResult(tool_name(env)),
            raw: best_text(env),
        }),
        "Stop" => Some(EventClass::Turn {
            role: TurnRole::Agent,
            kind: TurnKind::AgentOutput,
            raw: best_text(env),
        }),
        _ => None,
    }
}

fn tool_name(env: &JournalEnvelope) -> String {
    env.tool.clone().unwrap_or_else(|| "unknown".into())
}

/// Best human-meaningful text on the envelope, in priority order.
fn best_text(env: &JournalEnvelope) -> String {
    env.response_preview
        .clone()
        .or_else(|| env.message.clone())
        .or_else(|| env.description.clone())
        .or_else(|| env.target.clone())
        .or_else(|| env.tool.clone())
        .unwrap_or_default()
}

fn build_metadata(env: &JournalEnvelope) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if let Some(t) = &env.tool {
        map.insert("tool".into(), serde_json::Value::String(t.clone()));
    }
    if let Some(t) = &env.target {
        map.insert("target".into(), serde_json::Value::String(t.clone()));
    }
    if let Some(c) = env.exit_code {
        map.insert("exit_code".into(), serde_json::Value::Number(c.into()));
    }
    if map.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::Object(map)
    }
}

fn parse_ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

#[cfg(test)]
#[path = "consumer_tests.rs"]
mod consumer_tests;

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Unified agent communication protocol.
//!
//! Four sources speak about an agent today — provider hooks
//! (Claude/Codex via `orkia bridge`), OSC 133 shell-integration
//! sequences in the byte stream, the structural state machine in
//! `terminal_state`, and (future) a native APC `Orkia` protocol —
//! and we want a single contract for all of them. This module is
//! the contract: every source converges into [`OrkiaEvent`], every
//! consumer reads [`OrkiaEvent`] and nothing else.
//!
//! **V1 lands as an adapter**: the existing journal / SEAL /
//! notification / pending-prompt drains keep working through their
//! current channels; the [`EventRouter`] is a NEW additive layer
//! that fans the same signals into a unified stream. Future
//! consumers (Surface app, dashboards, team-sync) plug into
//! [`EventRouter::take_rx`] — they never touch the legacy paths.
//!
//! V2 adds the `\x1b_Orkia;<base64 json>\x1b\\` APC protocol; V3
//! adds the agent SDK. See the plan file in `~/.claude/plans/`.

mod apc;
mod converter;
mod emitter;
mod hooks;
mod osc133;
mod state_machine;

pub use apc::parse_orkia_apc;
pub use converter::{EventRouter, FanoutConfig, Osc133Marker, spawn_fanout};
pub use emitter::emit_orkia_apc;
pub use hooks::convert_hook;
pub use osc133::parse_osc133;
pub use state_machine::convert_detector_event;

use serde::{Deserialize, Serialize};

pub use crate::terminal_state::PromptType;
use orkia_shell_types::JobId;

/// One event about an agent, regardless of how orkia heard about it.
/// `source` records the truth tier so consumers can choose to trust
/// or weight events differently; `confidence` collapses that into a
/// single scalar for simple gating.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrkiaEvent {
    pub source: EventSource,
    pub event: EventPayload,
    /// `1.0` for structured sources (`Osc133`, `Hook`,
    /// `OrkiaProtocol`); `0.6 - 0.95` for the `StateMachine`
    /// detector which is structural-but-inferred.
    pub confidence: f32,
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub job_id: JobId,
    pub agent_name: String,
    /// RFC context active in the REPL at the moment the event was
    /// constructed. `Some(rfc_id)` means the event will contribute to
    /// that RFC's SEAL v1 document at closure; `None` means the event
    /// happened outside any RFC scope (the default for all event
    /// sources except REPL-stamped emissions).
    ///
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rfc_id: Option<orkia_rfc_core::RfcId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(tag = "kind")]
pub enum EventSource {
    /// Native APC protocol — `\x1b_Orkia;...\x1b\\`. V2.
    OrkiaProtocol,
    /// FinalTerm shell-integration OSC 133 A/B/C/D markers.
    Osc133,
    /// Provider hook (Claude / Codex / Gemini) via `orkia bridge`.
    Hook { provider: String },
    /// Structural prompt detection by `terminal_state`. Inferred.
    StateMachine,
    /// REPL-internal emission — RFC builtins, approval handlers,
    /// `agent.spawn` metadata, etc. Always carries `EventPayload::Custom`
    /// and bypasses the dedup window (`EventRouter::on_custom`); these
    /// are first-party records that no other source duplicates.
    Internal,
}

impl EventSource {
    /// Higher priority sources suppress lower-priority overlapping
    /// events within the dedup window (see [`EventRouter`]).
    /// `Internal` sits at the top because it's the deliberate
    /// REPL voice — never something we want suppressed by a
    /// racing hook from the same job.
    pub fn priority(&self) -> u8 {
        match self {
            EventSource::Internal => 5,
            EventSource::OrkiaProtocol => 4,
            EventSource::Osc133 => 3,
            EventSource::Hook { .. } => 2,
            EventSource::StateMachine => 1,
        }
    }
}

/// The actual semantic content of an event. Variants align with the
/// shell prompt cycle (OSC 133), hook lifecycle (Claude/Codex), and
/// state-machine detections so every source has a natural target.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EventPayload {
    // ── Lifecycle ─────────────────────────────────────────────
    SessionStart {
        model: Option<String>,
    },
    SessionEnd {
        exit_code: Option<i32>,
    },

    // ── Prompt cycle (maps 1:1 to OSC 133 A/B/C/D) ───────────
    PromptStart,
    PromptReady,
    CommandStart {
        command: String,
    },
    OutputStart,
    OutputFinished {
        exit_code: Option<i32>,
    },

    // ── Tool use (Claude/Codex hooks) ────────────────────────
    ToolUse {
        tool: String,
        target: Option<String>,
        input_summary: Option<String>,
    },
    ToolResult {
        tool: String,
        target: Option<String>,
        exit_code: Option<i32>,
        output_summary: Option<String>,
    },

    // ── Permission ───────────────────────────────────────────
    PermissionRequest {
        tool: Option<String>,
        description: String,
        risk: Option<String>,
    },
    PermissionResolved {
        approved: bool,
        resolved_by: String,
    },

    // ── State-machine detection (no other source can produce) ─
    Attention {
        prompt_type: PromptType,
        last_line: String,
    },

    // ── Communication ─────────────────────────────────────────
    AgentMessage {
        text: String,
    },
    UserMessage {
        text: String,
    },

    // ── Protocol extensions (V2+) ────────────────────────────
    StateReport {
        state: serde_json::Value,
    },
    Request {
        action: String,
        params: serde_json::Value,
    },
    /// Catch-all for protocol extensions / unknown variants. Lets us
    /// forward-evolve the protocol without breaking older orkias.
    Custom {
        name: String,
        data: serde_json::Value,
    },
}

impl EventPayload {
    /// Short identifier used for dedup keys (same semantic intent
    /// from different sources should share a tag).
    pub fn tag(&self) -> &'static str {
        match self {
            EventPayload::SessionStart { .. } => "session_start",
            EventPayload::SessionEnd { .. } => "session_end",
            EventPayload::PromptStart => "prompt_start",
            EventPayload::PromptReady => "prompt_ready",
            EventPayload::CommandStart { .. } => "command_start",
            EventPayload::OutputStart => "output_start",
            EventPayload::OutputFinished { .. } => "output_finished",
            EventPayload::ToolUse { .. } => "tool_use",
            EventPayload::ToolResult { .. } => "tool_result",
            EventPayload::PermissionRequest { .. } => "permission_request",
            EventPayload::PermissionResolved { .. } => "permission_resolved",
            EventPayload::Attention { .. } => "attention",
            EventPayload::AgentMessage { .. } => "agent_message",
            EventPayload::UserMessage { .. } => "user_message",
            EventPayload::StateReport { .. } => "state_report",
            EventPayload::Request { .. } => "request",
            EventPayload::Custom { .. } => "custom",
        }
    }
}

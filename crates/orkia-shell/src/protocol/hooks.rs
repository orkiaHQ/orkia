// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `JournalEnvelope` (hook source) → [`OrkiaEvent`] converter.
//!
//! Hooks arrive over the Unix socket at `<data_dir>/run/orkia.sock`
//! after being normalized by the `orkia bridge` binary. The
//! envelope is permissive — many fields are `Option<String>` —
//! because each provider (Claude / Codex / Gemini) populates a
//! different subset. We convert what we can; unknown hook events
//! become `Custom` (so consumers see them) or `None` (when the
//! envelope is not a hook at all).

use orkia_shell_types::JobId;

use super::{EventPayload, EventSource, OrkiaEvent};
use crate::journal::{EventType, JournalEnvelope};

/// Translate a journal envelope to an `OrkiaEvent`. Returns `None`
/// for envelopes we know are not user-facing (shell ticks, seal
/// bridge entries) — they stay journal-only.
pub fn convert_hook(env: &JournalEnvelope) -> Option<OrkiaEvent> {
    convert_hook_with_rfc(env, None)
}

pub fn convert_hook_with_rfc(
    env: &JournalEnvelope,
    rfc_id: Option<orkia_rfc_core::RfcId>,
) -> Option<OrkiaEvent> {
    if env.event_type != EventType::Hook {
        return None;
    }
    let name = env.event.as_deref()?;
    let payload = match name {
        "SessionStart" | "SessionStarted" => EventPayload::SessionStart {
            model: env.model.clone(),
        },
        "Stop" => EventPayload::SessionEnd {
            exit_code: env.exit_code,
        },
        "PreToolUse" => EventPayload::ToolUse {
            tool: env.tool.clone().unwrap_or_default(),
            target: env.target.clone(),
            input_summary: env.description.clone(),
        },
        "PostToolUse" => EventPayload::ToolResult {
            tool: env.tool.clone().unwrap_or_default(),
            target: env.target.clone(),
            exit_code: env.exit_code,
            output_summary: env.description.clone(),
        },
        "PermissionRequest" => EventPayload::PermissionRequest {
            tool: env.tool.clone(),
            description: env.description.clone().unwrap_or_default(),
            risk: env.risk.clone(),
        },
        "UserPromptSubmit" => EventPayload::CommandStart {
            command: env.prompt.clone().unwrap_or_default(),
        },
        "Notification" => EventPayload::AgentMessage {
            text: env.message.clone().unwrap_or_default(),
        },
        // Unknown hook event → forward as Custom so consumers can
        // still observe it. The catch-all also future-proofs us
        // against new hook names without rev'ing the protocol.
        other => EventPayload::Custom {
            name: other.to_string(),
            data: serde_json::to_value(env)
                .unwrap_or_else(|_| serde_json::Value::String("<unserializable envelope>".into())),
        },
    };
    Some(OrkiaEvent {
        source: EventSource::Hook {
            provider: env.source.clone().unwrap_or_default(),
        },
        event: payload,
        confidence: 1.0,
        timestamp: chrono::Utc::now(),
        job_id: JobId(env.job_id.unwrap_or(0)),
        agent_name: env.agent.clone().unwrap_or_default(),
        rfc_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook_env(name: &str) -> JournalEnvelope {
        let mut e = JournalEnvelope::now(EventType::Hook);
        e.event = Some(name.into());
        e.job_id = Some(1);
        e.agent = Some("faye".into());
        e.source = Some("claude".into());
        e
    }

    #[test]
    fn session_start_carries_model() {
        let mut e = hook_env("SessionStart");
        e.model = Some("claude-opus-4-7".into());
        let evt = convert_hook(&e).expect("event");
        assert!(matches!(
            evt.event,
            EventPayload::SessionStart { model: Some(ref m) } if m == "claude-opus-4-7"
        ));
        assert_eq!(evt.confidence, 1.0);
        assert!(matches!(evt.source, EventSource::Hook { ref provider } if provider == "claude"));
    }

    #[test]
    fn pre_tool_use_carries_target() {
        let mut e = hook_env("PreToolUse");
        e.tool = Some("Read".into());
        e.target = Some("src/auth/mod.rs".into());
        let evt = convert_hook(&e).expect("event");
        match evt.event {
            EventPayload::ToolUse {
                ref tool,
                ref target,
                ..
            } => {
                assert_eq!(tool, "Read");
                assert_eq!(target.as_deref(), Some("src/auth/mod.rs"));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn post_tool_use_carries_exit_code() {
        let mut e = hook_env("PostToolUse");
        e.tool = Some("Bash".into());
        e.exit_code = Some(0);
        let evt = convert_hook(&e).expect("event");
        match evt.event {
            EventPayload::ToolResult { exit_code, .. } => assert_eq!(exit_code, Some(0)),
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn permission_request_includes_risk() {
        let mut e = hook_env("PermissionRequest");
        e.description = Some("rm -rf node_modules".into());
        e.risk = Some("medium".into());
        let evt = convert_hook(&e).expect("event");
        match evt.event {
            EventPayload::PermissionRequest {
                ref description,
                ref risk,
                ..
            } => {
                assert_eq!(description, "rm -rf node_modules");
                assert_eq!(risk.as_deref(), Some("medium"));
            }
            other => panic!("expected PermissionRequest, got {other:?}"),
        }
    }

    #[test]
    fn stop_carries_exit_code() {
        let mut e = hook_env("Stop");
        e.exit_code = Some(0);
        let evt = convert_hook(&e).expect("event");
        assert!(matches!(
            evt.event,
            EventPayload::SessionEnd { exit_code: Some(0) }
        ));
    }

    #[test]
    fn unknown_hook_becomes_custom() {
        let e = hook_env("SomeNewHook");
        let evt = convert_hook(&e).expect("event");
        match evt.event {
            EventPayload::Custom { ref name, .. } => assert_eq!(name, "SomeNewHook"),
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn non_hook_envelopes_return_none() {
        let mut e = JournalEnvelope::now(EventType::Tell);
        e.event = Some("anything".into());
        assert!(convert_hook(&e).is_none());

        let mut e = JournalEnvelope::now(EventType::Lifecycle);
        e.event = Some("spawn".into());
        assert!(convert_hook(&e).is_none());
    }
}

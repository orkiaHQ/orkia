// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Best-effort normalisation of provider-native hook JSON into a
//! [`JournalEnvelope`].
//!
//! Used in two places:
//!
//! 1. **`orkia bridge --source <provider>`** — the canonical
//!    ingestion path. Provider hook configs invoke this; it reads
//!    raw stdin, calls `normalize_hook_value`, and writes the
//!    envelope to the journal socket.
//!
//! 2. **Journal listener fallback** — when strict
//!    `JournalEnvelope` deserialisation fails AND the raw line
//!    contains `hook_event_name`, the listener calls this to
//!    recover. Lets the live shell stay useful even if the user's
//!    hook config points to an out-of-date `orkia-bridge` binary
//!    that doesn't normalise (a real failure mode seen in the
//!    field — old installs in `~/.orkia/bin/orkia-bridge` did
//!    pass-through writes).
//!
//! The function is intentionally permissive: missing fields become
//! `None`, the only hard requirement is "looks like a hook"
//! (presence of `event` or `hook_event_name`). Anything malformed
//! falls back to `None` so the listener can log and drop instead
//! of crashing.

use orkia_shell_types::input_limits::JOURNAL_LINE_MAX_BYTES;
use orkia_shell_types::journal::types::{EventType, JournalEnvelope};

/// Upper bound on an inlined `last_assistant_message`. Half the journal
/// line cap, leaving generous headroom for the rest of the Stop envelope
/// (session_id, paths, model, …) so stashing the final text can never push
/// the line past `JOURNAL_LINE_MAX_BYTES` and get the whole Stop dropped.
const STOP_FINAL_MESSAGE_MAX_BYTES: usize = JOURNAL_LINE_MAX_BYTES / 2;

/// Build a `JournalEnvelope` from a provider's raw hook JSON value.
/// Returns `None` when the value doesn't even contain an event
/// name (i.e. not a hook at all). Provider-specific event names
/// (`BeforeTool`, `SessionEnd`) are folded to their canonical
/// counterparts (`PreToolUse`, `Stop`).
pub fn normalize_hook_value(source: &str, raw: &serde_json::Value) -> Option<JournalEnvelope> {
    let event_name = raw
        .get("event")
        .or_else(|| raw.get("hook_event_name"))
        .and_then(|v| v.as_str())?
        .to_string();
    let event_name = normalize_event_name(source, &event_name).to_string();

    let tool = raw
        .get("tool_name")
        .or_else(|| raw.get("tool"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let target = extract_target(raw);

    // The bridge process inherits env from the agent process, so
    // `ORKIA_JOB_ID` / `ORKIA_AGENT_NAME` set at spawn time are
    // present. The listener-recovery path runs in the orkia
    // process and won't see them; that's fine — the journal store
    // will fill in `agent` from `job_id` later via `enrich_envelope`.
    let job_id = std::env::var("ORKIA_JOB_ID")
        .ok()
        .and_then(|s| s.parse::<u32>().ok());
    let agent = std::env::var("ORKIA_AGENT_NAME").ok();

    // Stash provider-carried fields the typed envelope does not model
    // explicitly. The final-response service reads `transcript_path`
    // and `cwd` from here to locate provider-specific log files
    let mut extra = serde_json::Map::new();
    for key in ["transcript_path", "cwd"] {
        if let Some(v) = raw.get(key).and_then(|v| v.as_str()) {
            extra.insert(key.to_string(), serde_json::Value::String(v.to_string()));
        }
    }
    // Claude's Stop hook delivers the turn's final assistant text inline as
    // `last_assistant_message`. Carrying it lets the final-response service
    // capture the turn directly, without racing (or depending on) the
    // transcript-file flush. Only stash it when it fits comfortably under
    // the journal line cap (`JOURNAL_LINE_MAX_BYTES`, 256 KiB) — an
    // oversize Stop line is dropped wholesale by the listener, which would
    // lose the Stop event itself. Past the cap we omit it and let the
    // extractor fall back to the on-disk transcript (the only correct
    // source for a response that large anyway).
    if let Some(v) = raw.get("last_assistant_message").and_then(|v| v.as_str())
        && v.len() <= STOP_FINAL_MESSAGE_MAX_BYTES
    {
        extra.insert(
            "last_assistant_message".to_string(),
            serde_json::Value::String(v.to_string()),
        );
    }

    Some(JournalEnvelope {
        event_type: EventType::Hook,
        timestamp: chrono::Utc::now().to_rfc3339(),
        // Stamped (if at all) by the hub fanout downstream of recovery, not here.
        hub_seq: None,
        job_id,
        session_id: raw
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(String::from),
        source: Some(source.to_string()),
        agent,
        event: Some(event_name),
        tool,
        target,
        exit_code: raw
            .get("exit_code")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32),
        action: raw.get("action").and_then(|v| v.as_str()).map(String::from),
        risk: raw.get("risk").and_then(|v| v.as_str()).map(String::from),
        description: raw
            .get("permission_description")
            .or_else(|| raw.get("description"))
            .and_then(|v| v.as_str())
            .map(String::from),
        message: raw
            .get("message")
            .and_then(|v| v.as_str())
            .map(String::from),
        model: raw.get("model").and_then(|v| v.as_str()).map(String::from),
        prompt: raw.get("prompt").and_then(|v| v.as_str()).map(String::from),
        response_path: None,
        response_sha256: None,
        response_bytes: None,
        pipeline_id: None,
        stage_index: None,
        response_preview: None,
        extra,
    })
}

/// Listener-side recovery: try strict parse first; if it fails AND
/// the line looks like a raw hook (`hook_event_name` present),
/// build the envelope via `normalize_hook_value`. The source is
/// inferred from the `_source` field claude writes, defaulting to
/// `"claude"` when missing (the historically-dominant client).
pub fn try_recover_hook_line(line: &str) -> Option<JournalEnvelope> {
    let raw: serde_json::Value = serde_json::from_str(line).ok()?;
    // Only attempt recovery for objects with an event name.
    raw.get("hook_event_name")?;
    let source = raw
        .get("_source")
        .and_then(|v| v.as_str())
        .unwrap_or("claude")
        .to_string();
    normalize_hook_value(&source, &raw)
}

/// Provider-specific event names → canonical names. Gemini fires
/// `BeforeTool` / `AfterTool` / `SessionEnd` where Claude / Codex
/// use `PreToolUse` / `PostToolUse` / `Stop`.
pub fn normalize_event_name<'a>(_source: &str, event: &'a str) -> &'a str {
    match event {
        "BeforeTool" => "PreToolUse",
        "AfterTool" => "PostToolUse",
        "SessionEnd" => "Stop",
        other => other,
    }
}

fn extract_target(raw: &serde_json::Value) -> Option<String> {
    let input = raw.get("tool_input").or_else(|| raw.get("input"))?;
    if let Some(path) = input
        .get("file_path")
        .or_else(|| input.get("path"))
        .and_then(|v| v.as_str())
    {
        return Some(short_path(path));
    }
    if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
        return Some(truncate(cmd, 64));
    }
    None
}

fn short_path(p: &str) -> String {
    // Keep last 2 path segments so notifications stay narrow but
    // remain disambiguating.
    if let Ok(cwd) = std::env::current_dir()
        && let Ok(stripped) = std::path::Path::new(p).strip_prefix(&cwd)
    {
        return stripped.display().to_string();
    }
    let segs: Vec<&str> = p.rsplit('/').take(2).collect();
    segs.into_iter().rev().collect::<Vec<_>>().join("/")
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_native_hook_recovers() {
        let line = r#"{"session_id":"abc","_source":"claude","_ppid":12345,"hook_event_name":"PreToolUse","tool_name":"Read","tool_input":{"file_path":"/tmp/x.rs"}}"#;
        let env = try_recover_hook_line(line).expect("recover");
        assert_eq!(env.event_type, EventType::Hook);
        assert_eq!(env.event.as_deref(), Some("PreToolUse"));
        assert_eq!(env.source.as_deref(), Some("claude"));
        assert_eq!(env.tool.as_deref(), Some("Read"));
        assert_eq!(env.session_id.as_deref(), Some("abc"));
        assert!(env.target.is_some(), "target derived from tool_input");
    }

    #[test]
    fn user_prompt_submit_carries_prompt() {
        let line =
            r#"{"_source":"claude","hook_event_name":"UserPromptSubmit","prompt":"fix the tests"}"#;
        let env = try_recover_hook_line(line).expect("recover");
        assert_eq!(env.event.as_deref(), Some("UserPromptSubmit"));
        assert_eq!(env.prompt.as_deref(), Some("fix the tests"));
    }

    #[test]
    fn gemini_event_renames() {
        let line = r#"{"_source":"gemini","hook_event_name":"BeforeTool","tool_name":"Bash"}"#;
        let env = try_recover_hook_line(line).expect("recover");
        assert_eq!(env.event.as_deref(), Some("PreToolUse"));
        assert_eq!(env.source.as_deref(), Some("gemini"));
    }

    #[test]
    fn missing_event_name_returns_none() {
        let line = r#"{"session_id":"x"}"#;
        assert!(try_recover_hook_line(line).is_none());
    }

    #[test]
    fn malformed_json_returns_none() {
        assert!(try_recover_hook_line("{not json").is_none());
    }

    #[test]
    fn stop_carries_last_assistant_message() {
        let line = r#"{"_source":"claude","hook_event_name":"Stop","last_assistant_message":"PONG"}"#;
        let env = try_recover_hook_line(line).expect("recover");
        assert_eq!(env.event.as_deref(), Some("Stop"));
        assert_eq!(
            env.extra
                .get("last_assistant_message")
                .and_then(|v| v.as_str()),
            Some("PONG")
        );
    }

    #[test]
    fn oversize_last_assistant_message_is_omitted() {
        let big = "x".repeat(STOP_FINAL_MESSAGE_MAX_BYTES + 1);
        let raw = serde_json::json!({
            "_source": "claude",
            "hook_event_name": "Stop",
            "last_assistant_message": big,
        });
        let env = normalize_hook_value("claude", &raw).expect("normalize");
        // Over the cap → not stashed; the extractor falls back to the file.
        assert!(env.extra.get("last_assistant_message").is_none());
    }

    #[test]
    fn permission_request_carries_risk() {
        let line = r#"{"_source":"claude","hook_event_name":"PermissionRequest","risk":"high","description":"rm -rf node_modules"}"#;
        let env = try_recover_hook_line(line).expect("recover");
        assert_eq!(env.event.as_deref(), Some("PermissionRequest"));
        assert_eq!(env.risk.as_deref(), Some("high"));
        assert_eq!(env.description.as_deref(), Some("rm -rf node_modules"));
    }
}

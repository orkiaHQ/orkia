// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `orkia bridge` subcommand: shim from agent hooks to the journal.
//!
//! Agent CLIs (Claude Code, Codex, Gemini) configure hooks of the form
//! `orkia bridge --source claude`. When a hook fires, the agent invokes
//! that command with a provider-specific JSON payload on stdin. This
//! shim:
//!
//! 1. reads stdin to EOF
//! 2. parses the payload (best-effort — a non-JSON line still produces
//!    an envelope with the raw text in `extra.raw`)
//! 3. enriches it with `ORKIA_JOB_ID` and `ORKIA_AGENT_NAME` from env
//!    (already exported by `SpawnAgent` at job spawn time)
//! 4. normalizes the event name across providers
//! 5. connects to `<data_dir>/run/orkia.sock` and writes one NDJSON line
//!
//! The shim exits 0 even when the socket is missing — a hook that
//! exits non-zero would block the agent. Failures are logged via
//! `tracing` only.

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use orkia_shell::journal::{EventType, JournalEnvelope};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::time::timeout;

/// Parsed flags for `orkia bridge`. Mandatory `--source <name>`;
/// job and agent come from the env injected by `SpawnAgent`.
/// `--scope job` marks the hook entry orkia generates into an agent's
/// project settings — the sole forwarder for an orkia-managed session.
#[derive(Debug)]
pub struct BridgeArgs {
    pub source: String,
    pub job_scoped: bool,
}

impl BridgeArgs {
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut source: Option<String> = None;
        let mut job_scoped = false;
        let mut iter = args.iter();
        while let Some(a) = iter.next() {
            match a.as_str() {
                "--source" => {
                    let v = iter
                        .next()
                        .ok_or_else(|| "--source requires a value".to_string())?;
                    source = Some(v.clone());
                }
                "--scope" => {
                    let v = iter
                        .next()
                        .ok_or_else(|| "--scope requires a value".to_string())?;
                    job_scoped = v == "job";
                }
                "-h" | "--help" => return Err("__help__".into()),
                other => return Err(format!("unknown argument: {other}")),
            }
        }
        let source = source.ok_or_else(|| "missing required --source <name>".to_string())?;
        Ok(Self { source, job_scoped })
    }
}

pub fn print_help() {
    eprintln!(
        "Usage: orkia bridge --source <claude|codex|gemini|generic> [--scope job]

Reads a hook payload on stdin and forwards it to the orkia journal
socket at ~/.orkia/run/orkia.sock. Used by agent hook configurations;
not intended to be run directly.

    --scope job     Marks the hook entry orkia generates into an
                    agent's project settings. Without it, the bridge
                    exits silently when ORKIA_SOCKET_PATH is set —
                    a global settings hook firing on an orkia-managed
                    session would otherwise duplicate every record.

Environment variables consumed (set automatically by `SpawnAgent`):
    ORKIA_JOB_ID        Numeric job id of the spawning agent.
    ORKIA_AGENT_NAME    Agent name (e.g. \"faye\").

Exit code is always 0: a hook that exits non-zero would block the
agent. Bridge failures are logged via tracing only."
    );
}

/// Entry point used by `main`. Awaited from the existing `#[tokio::main]`
/// runtime; always returns 0 — see module docs.
pub async fn run(args: &BridgeArgs) -> i32 {
    run_async(args).await;
    0
}

async fn run_async(args: &BridgeArgs) {
    // One forwarder per session. An orkia-managed agent (it exports
    // `ORKIA_SOCKET_PATH` at spawn) already carries a `--scope job`
    // hook in its generated project settings; any OTHER invocation
    // against the same session is the user's global settings hook
    // firing in duplicate. Forwarding it would double every journal
    // and SEAL record and race the final-response ledger — so the
    // global entry self-suppresses. Personal (non-orkia) sessions
    // have no `ORKIA_SOCKET_PATH` and forward exactly as before.
    if !args.job_scoped && std::env::var_os("ORKIA_SOCKET_PATH").is_some() {
        tracing::debug!("orkia bridge: global hook on orkia-managed session — suppressed");
        return;
    }
    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        tracing::warn!("orkia bridge: stdin read: {e}");
        return;
    }
    let raw: serde_json::Value =
        serde_json::from_str(input.trim()).unwrap_or_else(|_| serde_json::json!({"raw": input}));

    let envelope = normalize(&args.source, &raw);
    if let Err(e) = send(&envelope).await {
        tracing::warn!("orkia bridge: send: {e}");
    }
}

/// Build a `JournalEnvelope` from the provider's raw hook JSON.
/// Thin wrapper that defers to the shared
/// [`orkia_shell::journal::normalize_hook_value`]. Returns an empty
/// `Hook` envelope when the JSON isn't a recognisable hook (no
/// `event` / `hook_event_name`) — historically the bridge always
/// produced *something* so the journal would still record the call.
pub fn normalize(source: &str, raw: &serde_json::Value) -> JournalEnvelope {
    orkia_shell::journal::normalize_hook_value(source, raw).unwrap_or_else(|| JournalEnvelope {
        event_type: EventType::Hook,
        timestamp: chrono::Utc::now().to_rfc3339(),
        source: Some(source.to_string()),
        ..Default::default()
    })
}

async fn send(envelope: &JournalEnvelope) -> std::io::Result<()> {
    let socket_path = socket_path();
    let mut stream = timeout(
        Duration::from_millis(500),
        UnixStream::connect(&socket_path),
    )
    .await??;
    let mut line = serde_json::to_string(envelope)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    timeout(
        Duration::from_millis(500),
        stream.write_all(line.as_bytes()),
    )
    .await??;
    timeout(Duration::from_millis(500), stream.shutdown()).await??;
    Ok(())
}

fn socket_path() -> PathBuf {
    // A detached agent runtime exports `ORKIA_SOCKET_PATH` pointing at its
    // per-job hub (`<data_dir>/run/jobs/<id>/agent.sock`) so the runtime
    // consumes its OWN agent's hooks instead of routing them to the global
    // readiness/trust-auto-answer state machine never sees SessionStart and
    // the initial prompt is never injected). Same env-injection contract the
    // shim already relies on for `ORKIA_JOB_ID`/`ORKIA_AGENT_NAME`. Absent
    // (main REPL / non-detached) → the global socket, unchanged.
    if let Some(path) = std::env::var_os("ORKIA_SOCKET_PATH") {
        return PathBuf::from(path);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".orkia").join("run").join("orkia.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_requires_source() {
        let err = BridgeArgs::parse(&[]).unwrap_err();
        assert!(err.contains("--source"));
    }

    #[test]
    fn parse_extracts_source() {
        let args = BridgeArgs::parse(&["--source".into(), "claude".into()]).expect("parse");
        assert_eq!(args.source, "claude");
    }

    #[test]
    fn parse_rejects_unknown_flag() {
        let err = BridgeArgs::parse(&["--bogus".into()]).unwrap_err();
        assert!(err.contains("unknown"));
    }

    #[test]
    fn parse_defaults_to_global_scope() {
        let args = BridgeArgs::parse(&["--source".into(), "claude".into()]).expect("parse");
        assert!(!args.job_scoped);
    }

    #[test]
    fn parse_scope_job_sets_job_scoped() {
        let args = BridgeArgs::parse(&[
            "--source".into(),
            "claude".into(),
            "--scope".into(),
            "job".into(),
        ])
        .expect("parse");
        assert!(args.job_scoped);
    }

    #[test]
    fn normalize_pulls_event_and_tool() {
        let raw = serde_json::json!({
            "event": "PreToolUse",
            "tool_name": "Read",
            "tool_input": {"file_path": "/tmp/example.rs"},
        });
        let env = normalize("claude", &raw);
        assert_eq!(env.event_type, EventType::Hook);
        assert_eq!(env.source.as_deref(), Some("claude"));
        assert_eq!(env.event.as_deref(), Some("PreToolUse"));
        assert_eq!(env.tool.as_deref(), Some("Read"));
        assert!(env.target.is_some());
    }

    #[test]
    fn normalize_gemini_event_names() {
        let raw = serde_json::json!({"event": "BeforeTool", "tool_name": "Read"});
        let env = normalize("gemini", &raw);
        assert_eq!(env.event.as_deref(), Some("PreToolUse"));

        let raw = serde_json::json!({"event": "AfterTool"});
        let env = normalize("gemini", &raw);
        assert_eq!(env.event.as_deref(), Some("PostToolUse"));

        let raw = serde_json::json!({"event": "SessionEnd"});
        let env = normalize("gemini", &raw);
        assert_eq!(env.event.as_deref(), Some("Stop"));
    }

    #[test]
    fn normalize_uses_hook_event_name_fallback() {
        let raw = serde_json::json!({"hook_event_name": "PostToolUse"});
        let env = normalize("codex", &raw);
        assert_eq!(env.event.as_deref(), Some("PostToolUse"));
    }

    #[test]
    fn normalize_extracts_command_target() {
        let raw = serde_json::json!({
            "event": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "cargo test --all"},
        });
        let env = normalize("claude", &raw);
        assert_eq!(env.target.as_deref(), Some("cargo test --all"));
    }

    #[test]
    fn normalize_with_env_picks_up_job_id() {
        // SAFETY: the test sets process-wide env vars. These names are
        // not used elsewhere in this test binary.
        unsafe {
            std::env::set_var("ORKIA_JOB_ID", "7");
            std::env::set_var("ORKIA_AGENT_NAME", "faye");
        }
        let env = normalize("claude", &serde_json::json!({"event": "Stop"}));
        assert_eq!(env.job_id, Some(7));
        assert_eq!(env.agent.as_deref(), Some("faye"));
        // SAFETY: same ORKIA_* names; restoring (clearing) them on
        // exit. Test does not run in parallel with anything that
        // reads these vars.
        unsafe {
            std::env::remove_var("ORKIA_JOB_ID");
            std::env::remove_var("ORKIA_AGENT_NAME");
        }
    }
}

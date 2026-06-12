// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Every evaluated command produces one `cage.verdict` record on the agent's
//! SEAL job chain. The shim builds a [`JournalEnvelope`] and writes it as one
//! NDJSON line **directly** to `$HOME/.orkia/run/orkia.sock`. The
//! `ORKIA_JOB_ID` / `ORKIA_AGENT_NAME` the cage injected at spawn ride the child
//! env and stamp the routing fields, so the record attaches to the right chain.
//! The listener parses the line (`event_type = Hook`, `event =
//! "cage.verdict"`), the protocol converter buckets it as a `Custom` event, and
//!
//! `orkia` binary, which is *not* present in the Linux minimal rootfs — and
//! binding a ~90 MB shell binary into the cage purely to write one socket line
//! this exact alternative ("the shim writes the NDJSON line directly to the
//! socket itself") for the unreachable-bridge case. Emission is just an event
//! write, so it is platform-agnostic — Linux (rootfs socket bind) or macOS
//! (socket directly).

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use orkia_shell_types::{EventType, JournalEnvelope, Verdict};
use serde_json::json;

/// Cage env carrying the spawning job's id / agent name (set by the launcher).
const JOB_ID_ENV: &str = "ORKIA_JOB_ID";
const AGENT_ENV: &str = "ORKIA_AGENT_NAME";
/// Stable project id (git-root path) the launcher injects so a `cage.verdict`
/// can be scoped. Absent ⇒ the verdict has no `project` and the scorer ignores it.
const PROJECT_ENV: &str = "ORKIA_PROJECT_ID";

/// Bound the socket write so a wedged listener can never hang the agent's hot
/// path. A timed-out write surfaces as `Err`, which fail-closes an `allow`.
const WRITE_TIMEOUT: Duration = Duration::from_millis(500);

/// Build the `cage.verdict` envelope **without** routing fields. Pure — the
/// caller stamps `job_id`/`agent` from the cage env. The four verdict fields
/// land in `extra`, which serde-flattens to the top level so the SEAL record's
/// `detail.verdict` / `detail.capability` read directly.
pub fn build_envelope(
    command: &str,
    verdict: Verdict,
    capability: Option<&str>,
    rule: Option<&str>,
) -> JournalEnvelope {
    let mut env = JournalEnvelope::now(EventType::Hook);
    env.event = Some("cage.verdict".to_string());
    env.source = Some("generic".to_string());
    env.extra.insert("command".to_string(), json!(command));
    env.extra
        .insert("verdict".to_string(), json!(verdict_str(verdict)));
    env.extra
        .insert("capability".to_string(), json!(capability));
    env.extra.insert("rule".to_string(), json!(rule));
    env
}

fn verdict_str(v: Verdict) -> &'static str {
    match v {
        Verdict::Allow => "allow",
        Verdict::Ask => "ask",
        Verdict::Deny => "deny",
    }
}

/// The journal socket the cage's `~/.orkia/run` bind makes reachable.
fn socket_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".orkia").join("run").join("orkia.sock")
}

/// Emit the verdict by writing one NDJSON envelope line to the journal socket.
/// Returns `Err` if the socket can't be reached or the write fails/times out —
/// the caller fail-closes an `allow` on that error (CLAUDE.md #8: audit-write
/// failure aborts the call).
pub fn emit(
    command: &str,
    verdict: Verdict,
    capability: Option<&str>,
    rule: Option<&str>,
) -> Result<()> {
    let mut env = build_envelope(command, verdict, capability, rule);
    stamp_routing(&mut env);
    send(&env)
}

/// Emit a `command.outcome` — the *result* of an allowed command (exit status),
/// the positive/negative trust signal that complements the decision. Polarity is
/// `success` (and the numeric `exit_code` when known). **Best-effort**: the
/// command has already run, so a failed audit write here changes nothing (unlike
/// an `allow` verdict, which gates execution and must be durable first).
pub fn emit_outcome(capability: Option<&str>, success: bool, exit_code: Option<i64>) {
    let mut env = build_outcome_envelope(capability, success, exit_code);
    stamp_routing(&mut env);
    let _ = send(&env);
}

/// Build the `command.outcome` envelope **without** routing fields (pure, like
/// [`build_envelope`]). `success`/`exit_code` flatten to `detail.success` /
/// `detail.exit_code`, the polarity the trust scorer reads.
fn build_outcome_envelope(
    capability: Option<&str>,
    success: bool,
    exit_code: Option<i64>,
) -> JournalEnvelope {
    let mut env = JournalEnvelope::now(EventType::Hook);
    env.event = Some("command.outcome".to_string());
    env.source = Some("generic".to_string());
    env.extra
        .insert("capability".to_string(), json!(capability));
    env.extra.insert("success".to_string(), json!(success));
    if let Some(code) = exit_code {
        env.extra.insert("exit_code".to_string(), json!(code));
    }
    env
}

/// Stamp the cage routing fields (`job_id`, `agent`, `project`) the launcher
/// injected — shared by `cage.verdict` and `command.outcome` so both carry the
/// same `(agent, project)` the trust scorer keys on.
fn stamp_routing(env: &mut JournalEnvelope) {
    env.job_id = std::env::var(JOB_ID_ENV)
        .ok()
        .and_then(|s| s.parse::<u32>().ok());
    env.agent = std::env::var(AGENT_ENV).ok();
    if let Ok(project) = std::env::var(PROJECT_ENV) {
        env.extra.insert("project".to_string(), json!(project));
    }
}

/// Write one NDJSON envelope line to the journal socket (timeout-bounded).
fn send(env: &JournalEnvelope) -> Result<()> {
    let mut line = serde_json::to_string(env).context("serialize journal envelope")?;
    line.push('\n');
    let path = socket_path();
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("connect journal socket {}", path.display()))?;
    stream
        .set_write_timeout(Some(WRITE_TIMEOUT))
        .context("set socket write timeout")?;
    stream
        .write_all(line.as_bytes())
        .context("write journal envelope to socket")?;
    Ok(())
}

/// Record a verdict where we will not gate on its success (e.g. a `deny` we are
/// about to enforce regardless). Best-effort: a failed audit here does not
/// change the already-fail-closed outcome.
pub fn emit_best_effort(
    command: &str,
    verdict: Verdict,
    capability: Option<&str>,
    rule: Option<&str>,
) {
    let _ = emit(command, verdict, capability, rule);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize the envelope and re-read as a generic JSON object so we can
    /// assert the wire shape the listener will parse (extra flattens to top).
    fn wire(env: &JournalEnvelope) -> serde_json::Value {
        serde_json::from_str(&serde_json::to_string(env).expect("serialize")).expect("reparse")
    }

    #[test]
    fn envelope_shape_matches_spec_deny() {
        let env = build_envelope(
            "git push origin main",
            Verdict::Deny,
            Some("git.push"),
            Some("git push*"),
        );
        let p = wire(&env);
        // `type` is the JournalEnvelope tag; the listener keys SEAL routing on
        // event_type = Hook + event = "cage.verdict".
        assert_eq!(p["type"], "hook");
        assert_eq!(p["event"], "cage.verdict");
        assert_eq!(p["source"], "generic");
        // Verdict detail (flattened from `extra`).
        assert_eq!(p["command"], "git push origin main");
        assert_eq!(p["verdict"], "deny");
        assert_eq!(p["capability"], "git.push");
        assert_eq!(p["rule"], "git push*");
    }

    #[test]
    fn envelope_default_match_has_null_capability_and_rule() {
        let p = wire(&build_envelope("some random cmd", Verdict::Ask, None, None));
        assert_eq!(p["verdict"], "ask");
        assert!(p["capability"].is_null());
        assert!(p["rule"].is_null());
    }

    #[test]
    fn verdict_strings_are_lowercase() {
        assert_eq!(verdict_str(Verdict::Allow), "allow");
        assert_eq!(verdict_str(Verdict::Ask), "ask");
        assert_eq!(verdict_str(Verdict::Deny), "deny");
    }

    #[test]
    fn outcome_envelope_carries_success_and_exit_code() {
        let p = wire(&build_outcome_envelope(Some("git.push"), false, Some(1)));
        assert_eq!(p["type"], "hook");
        assert_eq!(p["event"], "command.outcome");
        assert_eq!(p["capability"], "git.push");
        assert_eq!(p["success"], false);
        assert_eq!(p["exit_code"], 1);
    }

    #[test]
    fn outcome_envelope_omits_exit_code_when_unknown() {
        let p = wire(&build_outcome_envelope(Some("git.commit"), true, None));
        assert_eq!(p["success"], true);
        assert!(p.get("exit_code").is_none());
    }
}

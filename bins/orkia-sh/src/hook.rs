// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! On macOS the agent's Bash-tool shell binary is **not** overridable (it calls
//! `/bin/bash` by absolute path), and there is no mount namespace to redirect
//! it, so the only *supported* per-command gate is a PreToolUse hook. This is a
//! **best-effort, cooperative** layer: it fires only when the agent goes through
//! its tool protocol, and is bypassable by an autonomous `bash -c`. The kernel
//! guarantee on macOS remains the Seatbelt exec-deny (whole-binary). Per-command
//! string granularity is reliable on **Linux only** in V1.
//!
//! Protocol: stdin carries the PreToolUse JSON (`tool_name`, `tool_input`); we
//! decision, then on stdout return a PreToolUse permission decision: a deny is
//! conveyed via JSON (`permissionDecision: "deny"`), an allow defers (exit 0,
//! no output) to the normal permission flow.

use std::io::Read;

use anyhow::{Context, Result};
use orkia_shell_types::Verdict;
use serde_json::{Value, json};

use crate::core::{Decision, decide};
use crate::verdict;

/// Tool names whose command we mediate. (Codex/Gemini hook equivalents are
/// separate follow-ups — their tool/envelope shapes differ.)
const MEDIATED_TOOLS: &[&str] = &["Bash"];

/// Run the hook: read stdin JSON, decide, record, and print the verdict.
pub fn run() -> Result<()> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("reading PreToolUse JSON from stdin")?;
    let parsed: Value = serde_json::from_str(&input).unwrap_or(Value::Null);

    let tool = parsed
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !MEDIATED_TOOLS.contains(&tool) {
        return Ok(()); // not a tool we gate → defer (exit 0)
    }

    let Some(command) = parsed
        .get("tool_input")
        .and_then(|t| t.get("command"))
        .and_then(Value::as_str)
    else {
        // Bash tool but no readable command — best-effort layer: defer rather
        // than break the agent (the Seatbelt guarantee still applies).
        eprintln!("orkia-cage: hook could not read the Bash command; deferring");
        return Ok(());
    };

    // The PreToolUse hook entry lives in the project's `.claude/settings.json`,
    // which persists on disk and is read by every Claude session run there —
    // not only caged ones. When this session is NOT under the cage we DEFER:
    // the hook is a best-effort cooperative layer, never a global gate (the
    // macOS guarantee is the Seatbelt exec-deny). A policy that IS present but
    // unreadable still fails closed inside `decide`.
    if !caged() {
        return Ok(());
    }

    // PostToolUse fires *after* the command ran — record its outcome (the
    // result-quality trust signal, the macOS counterpart to Linux's fork+wait).
    // Every other event (PreToolUse) is the decision gate.
    match parsed.get("hook_event_name").and_then(Value::as_str) {
        Some("PostToolUse") => record_outcome(command, &parsed),
        _ => act(decide(command)),
    }
    Ok(())
}

/// PostToolUse: re-derive the capability from the command and emit a
/// `command.outcome` with the success the tool reported. Best-effort and
/// cooperative — the same posture as the PreToolUse verdict: only commands routed
/// through the tool protocol are seen, which is fail-safe (an agent that bypasses
/// the protocol earns *no* evidence rather than false evidence).
fn record_outcome(command: &str, parsed: &Value) {
    let capability = capability_of(decide(command));
    let (success, exit_code) = outcome_signal(parsed.get("tool_response"));
    verdict::emit_outcome(capability.as_deref(), success, exit_code);
}

/// The matched capability name for a command, whatever the verdict tier.
fn capability_of(d: Decision) -> Option<String> {
    match d {
        Decision::Allow { capability, .. }
        | Decision::Deny { capability, .. }
        | Decision::Ask { capability, .. } => capability,
    }
}

/// Read the success polarity from a PostToolUse `tool_response`, defensively
/// across the shapes agents use. A numeric `exit_code` is authoritative; else an
/// explicit error/interrupt flag means failure; absent any failure signal a
/// completed command is treated as success (best-effort — and farmed positives
/// only ever lift benign caps, never sensitive ones).
fn outcome_signal(resp: Option<&Value>) -> (bool, Option<i64>) {
    let Some(r) = resp else {
        return (true, None);
    };
    if let Some(code) = r.get("exit_code").and_then(Value::as_i64) {
        return (code == 0, Some(code));
    }
    let failed = r
        .get("interrupted")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || r.get("is_error").and_then(Value::as_bool).unwrap_or(false)
        || r.get("isError").and_then(Value::as_bool).unwrap_or(false);
    (!failed, None)
}

/// True when a cage policy is present in the env — i.e. this Claude session is
/// actually running under the cage (`orkia-cage` sets `ORKIA_CAGE_POLICY`). The
/// PreToolUse hook config persists in the project, so a session started outside
/// the cage must defer rather than fail-closed deny every command.
fn caged() -> bool {
    std::env::var_os(crate::core::POLICY_ENV).is_some()
}

/// Enforce + record the decision in PreToolUse terms. The three tiers are
/// separate arms (not an `Ask | Deny` merge): `Allow` defers, `Deny`/`Ask` both
/// deny in V1 — a future trust layer would widen the `Ask` arm alone.
fn act(d: Decision) {
    match d {
        Decision::Allow {
            command,
            capability,
            rule,
        } => {
            // Record the allow; fail-closed if the audit write fails
            // (CLAUDE.md #8) — convey that as a deny so nothing runs unaudited.
            match verdict::emit(
                &command,
                Verdict::Allow,
                capability.as_deref(),
                rule.as_deref(),
            ) {
                Ok(()) => {} // allow → defer (exit 0, no output)
                Err(e) => emit_decision("deny", &format!("audit write failed (fail-closed): {e}")),
            }
        }
        Decision::Deny {
            command,
            capability,
            rule,
            forced_reason,
        } => {
            verdict::emit_best_effort(
                &command,
                Verdict::Deny,
                capability.as_deref(),
                rule.as_deref(),
            );
            emit_decision("deny", &deny_reason(capability.as_deref(), forced_reason));
        }
        // V1: ask recorded as `ask`, enforced as deny. Own arm so a future trust
        // layer attaches here without touching the terminal Deny.
        Decision::Ask {
            command,
            capability,
            rule,
        } => {
            verdict::emit_best_effort(
                &command,
                Verdict::Ask,
                capability.as_deref(),
                rule.as_deref(),
            );
            emit_decision("deny", &deny_reason(capability.as_deref(), None));
        }
    }
}

fn deny_reason(capability: Option<&str>, forced: Option<&'static str>) -> String {
    match (capability, forced) {
        (Some(cap), _) => format!("DENIED by orkia-cage (capability: {cap})"),
        (None, Some(reason)) => format!("DENIED by orkia-cage ({reason})"),
        (None, None) => "DENIED by orkia-cage (no matching allow)".to_string(),
    }
}

/// Print a PreToolUse permission decision (the documented hook output shape).
fn emit_decision(decision: &str, reason: &str) {
    let out = json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": reason,
        }
    });
    println!("{out}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_reason_prefers_capability() {
        assert_eq!(
            deny_reason(Some("git.push"), None),
            "DENIED by orkia-cage (capability: git.push)"
        );
    }

    #[test]
    fn deny_reason_uses_forced_when_no_capability() {
        assert_eq!(
            deny_reason(None, Some("unparseable agent envelope")),
            "DENIED by orkia-cage (unparseable agent envelope)"
        );
    }

    #[test]
    fn caged_reflects_policy_env_presence() {
        // SAFETY: process-wide env mutation. `ORKIA_CAGE_POLICY` is read only
        // here and in `core::load_policy`, whose tests never set a parseable
        // command that would reach it — no concurrent reader in this binary.
        unsafe { std::env::remove_var(crate::core::POLICY_ENV) };
        assert!(!caged(), "no policy env → uncaged → defer");
        unsafe { std::env::set_var(crate::core::POLICY_ENV, "/x/policy.toml") };
        assert!(caged(), "policy env present → caged → mediate");
        unsafe { std::env::remove_var(crate::core::POLICY_ENV) };
    }

    #[test]
    fn outcome_signal_uses_exit_code_when_present() {
        assert_eq!(
            outcome_signal(Some(&json!({"exit_code": 0}))),
            (true, Some(0))
        );
        assert_eq!(
            outcome_signal(Some(&json!({"exit_code": 2}))),
            (false, Some(2))
        );
    }

    #[test]
    fn outcome_signal_treats_error_or_interrupt_as_failure() {
        assert_eq!(
            outcome_signal(Some(&json!({"is_error": true}))),
            (false, None)
        );
        assert_eq!(
            outcome_signal(Some(&json!({"interrupted": true}))),
            (false, None)
        );
    }

    #[test]
    fn outcome_signal_defaults_to_success_without_failure_signal() {
        assert_eq!(outcome_signal(Some(&json!({"stdout": "hi"}))), (true, None));
        assert_eq!(outcome_signal(None), (true, None));
    }

    #[test]
    fn capability_of_extracts_from_any_tier() {
        let d = Decision::Allow {
            command: "git push".into(),
            capability: Some("git.push".into()),
            rule: None,
        };
        assert_eq!(capability_of(d), Some("git.push".to_string()));
    }

    #[test]
    fn decision_json_is_well_formed() {
        let out = json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": "r",
            }
        });
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "deny");
    }
}

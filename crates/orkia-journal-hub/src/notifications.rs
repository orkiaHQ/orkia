// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Map journal envelopes onto ANSI notification lines + the
//! one-line summary used by `journal` query output.
//!
//! The REPL drains journal envelopes between prompts; for each one
//! that is user-facing we produce one short ANSI-coloured line. The
//! `ShellModeRenderer` prints these above the next prompt. TUI mode
//! ignores them — it has its own attached-job view.
//!
//! Returning `None` means "do not surface this event between prompts"
//! — the envelope still lands in `journal.jsonl` so a later
//! `journal --type hook` query sees it.

use orkia_shell_types::journal::types::{EventType, JournalEnvelope};

/// Render a notification line for `env`, or `None` for events that
/// should stay journal-only (raw shell ticks, session-start chatter,
/// etc.). Format: `  [job N agent] <verb> <subject>`.
pub fn notification_for(env: &JournalEnvelope) -> Option<String> {
    // Hooks from claude sessions not spawned by this orkia (no
    // `ORKIA_JOB_ID` env var → no `job_id` on the envelope) are
    // foreign agent activity — typically the user's own external
    // claude that has `orkia bridge` wired into its hook config
    // for SEAL/journal capture. Toasting that activity inside the
    // orkia shell scribbles unrelated work over the user's prompt.
    // The envelope still lands in the journal store and SEAL chain
    // (queryable via `journal --type hook` / `seal`); only the
    // live toast path is suppressed.
    if matches!(env.event_type, EventType::Hook) && env.job_id.is_none() {
        return None;
    }
    let tag = tag_for(env);
    match env.event_type {
        EventType::Hook => render_hook(env, &tag),
        EventType::Approval => {
            let action = env.action.as_deref().unwrap_or("approval");
            let risk = env.risk.as_deref().unwrap_or("?");
            Some(format!(
                "  {tag} \x1b[33m⚠ approval needed:\x1b[0m {action} (risk: {risk})"
            ))
        }
        EventType::Lifecycle => render_lifecycle(env, &tag),
        EventType::Tell => {
            // Tell envelopes from orkia internal paths (handle_tell,
            // emit_injection, deliver_to_existing_agent) already
            // produce a live toast at the call site (`▸ prompt
            // injected: …`, `▸ queued for [N]`, `tell: delivered to
            // job N`). Re-rendering the same fact here would double-
            // print the message above the next prompt — exactly the
            // "→ say hellooo" that appeared minutes after the live
            // injection. The envelope still lands in journal.jsonl
            // so `journal --type tell` queries return it.
            if env.source.as_deref() == Some("orkia") {
                return None;
            }
            let msg = env.message.as_deref().unwrap_or("");
            Some(format!("  {tag} \x1b[36m→\x1b[0m {}", truncate(msg, 80)))
        }
        // Shell ticks (every brush command) and SEAL records would
        // flood the prompt; they live only in the journal store.
        // ScopeChange envelopes are silent in PR1a — PR1b decides the
        // surface treatment alongside the emission helper.
        // KnowledgeAccess is bookkeeping for the reasoning consumer (the access
        // bump); it never surfaces to the prompt.
        EventType::Shell
        | EventType::Seal
        | EventType::ScopeChange
        | EventType::KnowledgeAccess => None,
    }
}

fn tag_for(env: &JournalEnvelope) -> String {
    match (env.job_id, env.agent.as_deref()) {
        (Some(id), Some(name)) => format!("\x1b[90m[job {id} {name}]\x1b[0m"),
        (Some(id), None) => format!("\x1b[90m[job {id}]\x1b[0m"),
        (None, Some(name)) => format!("\x1b[90m[{name}]\x1b[0m"),
        (None, None) => "\x1b[90m[orkia]\x1b[0m".into(),
    }
}

fn render_hook(env: &JournalEnvelope, tag: &str) -> Option<String> {
    let name = env.event.as_deref()?;
    match name {
        // Cage decisions. The agent typed a command; the cage ruled on
        // it before it ran. This is the one piece of agent traffic the
        // user genuinely wants to see live at the prompt while the
        // agent works in the background — it is the trust boundary
        // doing its job. We surface the policy-relevant ones only:
        // every `deny` (the agent was stopped), and every `allow` that
        // a capability rule explicitly matched. Default-allow traffic
        // (capability null — internal `git config`, the bridge, `ps`)
        // would flood the prompt, so it stays journal-only. Exit codes
        // live in `audit --verify`, which pairs each verdict with its
        // command outcome.
        "cage.verdict" => render_verdict(env, tag),
        "operator.drift_detected" => render_operator_drift(env, tag),
        "operator.cross_session_conflict" => render_operator_cross_session(env, tag),
        "operator.suggestion_created" => render_operator_suggestion(env, tag),
        // Routine tool-use traffic: autonomous internal flow of the
        // agent, nothing the user has to decide. Streaming a toast
        // per `Read` / `Bash` / `Grep` floods the prompt for any
        // non-trivial task (a 20-call sequence = 40 lines). Kept in
        // the journal store + SEAL chain so `journal --job N` and
        // `seal` still surface the full trace on demand. The user-
        // facing toasts are reserved for events that *require*
        // action (PermissionRequest) or *signal* the agent is
        // stuck waiting (Notification).
        "PreToolUse" | "PostToolUse" => None,
        "PermissionRequest" => {
            let action = env.action.as_deref().or(env.tool.as_deref()).unwrap_or("?");
            let risk = env.risk.as_deref().unwrap_or("?");
            Some(format!(
                "  {tag} \x1b[33m⚠ approval needed:\x1b[0m {action} (risk: {risk})"
            ))
        }
        // `Stop` and `SubagentStop` fire after *every claude turn*,
        // not at session exit. Surfacing them as `✓ completed`
        // misleads the user into thinking the agent is done — they
        // appeared after every prompt the user typed. The journal
        // still stores them for queries; just no toast. The actual
        // session-end signal is `JobEvent::Completed` which the
        // shell-mode renderer prints as `[N] done (exit N)`.
        "Stop" | "SubagentStop" => None,
        "Notification" => {
            let msg = env.message.as_deref().unwrap_or("");
            Some(format!("  {tag} {msg}"))
        }
        // SessionStart / UserPromptSubmit are noisy under shell mode.
        // The journal still records them; just no toast.
        _ => None,
    }
}

fn render_operator_drift(env: &JournalEnvelope, tag: &str) -> Option<String> {
    let severity = env
        .extra
        .get("severity")
        .and_then(|v| v.as_str())
        .unwrap_or("warning");
    let kind = env
        .extra
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("drift");
    let reason = env.message.as_deref()?;
    let (label, colour) = match severity {
        "critical" => ("operator critical", "\x1b[31m"),
        "warning" => ("operator drift", "\x1b[33m"),
        _ => ("operator notice", "\x1b[36m"),
    };
    Some(format!(
        "  {tag} {colour}⚠ {label}:\x1b[0m {kind} · {}",
        truncate(reason, 96)
    ))
}

fn render_operator_cross_session(env: &JournalEnvelope, tag: &str) -> Option<String> {
    let reason = env.message.as_deref()?;
    Some(format!(
        "  {tag} \x1b[35m↔ operator cross-session:\x1b[0m {}",
        truncate(reason, 104)
    ))
}

fn render_operator_suggestion(env: &JournalEnvelope, tag: &str) -> Option<String> {
    let reason = env.message.as_deref()?;
    Some(format!(
        "  {tag} \x1b[36m↳ operator suggestion:\x1b[0m {}",
        truncate(reason, 104)
    ))
}

/// Render a `cage.verdict` envelope as a live toast. The verdict detail
/// (`command` / `verdict` / `capability`) is flattened to the top level
/// by `orkia-sh`, so it arrives in `extra`. Returns `None` for default
/// allows (no capability matched) to keep the prompt readable.
fn render_verdict(env: &JournalEnvelope, tag: &str) -> Option<String> {
    let verdict = env.extra.get("verdict").and_then(|v| v.as_str())?;
    let command = env
        .extra
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let capability = env.extra.get("capability").and_then(|v| v.as_str());
    match verdict {
        "deny" => {
            let cap = capability.map(|c| format!(" {c}")).unwrap_or_default();
            Some(format!(
                "  {tag} \x1b[31m⛔ DENIED{cap}\x1b[0m · {}",
                truncate(command, 64)
            ))
        }
        // Only surface allows that a capability rule matched; default
        // allows (capability null) stay journal-only.
        "allow" => capability.map(|cap| {
            format!(
                "  {tag} \x1b[32m✓ allow {cap}\x1b[0m · {}",
                truncate(command, 64)
            )
        }),
        _ => None,
    }
}

fn render_lifecycle(_env: &JournalEnvelope, _tag: &str) -> Option<String> {
    // All lifecycle transitions (spawn, completed, stopped, continued,
    // attached, detached) are mirrored from the local `JobEvent`
    // stream into the journal, and the shell-mode renderer already
    // prints a line for each `JobEvent` it sees in
    // `renderers/shell_mode.rs::publish_job_update`. Emitting a second
    // line from the journal toast layer just duplicates output — the
    // user saw `[1] done (exit 1)` immediately followed by
    // `[job 1] ✗ failed (exit 1)` for the same completion. Keep this
    // path as a no-op; if we ever want a richer toast (e.g. multi-line
    // claude-side hooks) it can grow back here without re-introducing
    // the dupe.
    None
}

/// Render one envelope as a `journal` query row: timestamp, type, and
/// the most informative scalar field for that type. Plain text, no
/// ANSI — the caller decides whether to colour the output.
/// The short, lowercase label for an event type (`hook`, `tell`, …). Shared by
/// the text `query_row` and the migrated `journal` Command's table.
pub fn event_type_label(event_type: EventType) -> &'static str {
    match event_type {
        EventType::Hook => "hook",
        EventType::Approval => "approval",
        EventType::Lifecycle => "lifecycle",
        EventType::Shell => "shell",
        EventType::Tell => "tell",
        EventType::Seal => "seal",
        EventType::ScopeChange => "scope_change",
        EventType::KnowledgeAccess => "knowledge_access",
    }
}

/// The human one-line summary for an envelope (event/tool/action/message,
/// per event type). Shared by `query_row` and the `journal` Command so both
pub fn event_summary(env: &JournalEnvelope) -> String {
    match env.event_type {
        EventType::Hook => {
            let evt = env.event.as_deref().unwrap_or("-");
            let tool = env.tool.as_deref().unwrap_or("");
            let target = env.target.as_deref().unwrap_or("");
            format!("{evt} {tool} {target}").trim().to_string()
        }
        EventType::Approval => {
            let action = env.action.as_deref().unwrap_or("-");
            let risk = env.risk.as_deref().unwrap_or("?");
            format!("{action} (risk: {risk})")
        }
        EventType::Lifecycle => env.event.as_deref().unwrap_or("-").to_string(),
        EventType::Shell => env.action.as_deref().unwrap_or("-").to_string(),
        EventType::Tell => {
            let msg = env.message.as_deref().unwrap_or("");
            truncate(msg, 60)
        }
        EventType::Seal => env.event.as_deref().unwrap_or("-").to_string(),
        EventType::ScopeChange => env.event.as_deref().unwrap_or("-").to_string(),
        EventType::KnowledgeAccess => {
            format!("recalled {} node(s)", env.knowledge_access_ids().len())
        }
    }
}

pub fn query_row(env: &JournalEnvelope) -> String {
    let ty = event_type_label(env.event_type);
    let agent = env.agent.as_deref().unwrap_or("-");
    let job = env
        .job_id
        .map(|n| format!("{n}"))
        .unwrap_or_else(|| "-".into());
    let summary = event_summary(env);
    format!(
        "{ts}  {ty:9}  job:{job:>3}  {agent:12}  {summary}",
        ts = env.timestamp
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(event: &str) -> JournalEnvelope {
        let mut e = JournalEnvelope::now(EventType::Hook);
        e.event = Some(event.into());
        e.job_id = Some(1);
        e.agent = Some("faye".into());
        e
    }

    #[test]
    fn pre_and_post_tool_use_are_silent() {
        // Routine tool flow doesn't get a toast — too noisy. Still
        // journaled + sealed; query `journal --job N` for the
        // detail. See `render_hook` for the rationale.
        let mut pre = hook("PreToolUse");
        pre.tool = Some("Read".into());
        pre.target = Some("src/auth/mod.rs".into());
        assert!(notification_for(&pre).is_none());

        let mut post = hook("PostToolUse");
        post.tool = Some("Bash".into());
        post.target = Some("cargo test".into());
        post.exit_code = Some(0);
        assert!(notification_for(&post).is_none());
        post.exit_code = Some(1);
        assert!(notification_for(&post).is_none());
    }

    fn verdict(verdict: &str, capability: Option<&str>, command: &str) -> JournalEnvelope {
        let mut e = hook("cage.verdict");
        e.extra.insert("verdict".into(), serde_json::json!(verdict));
        e.extra.insert("command".into(), serde_json::json!(command));
        e.extra.insert(
            "capability".into(),
            match capability {
                Some(c) => serde_json::json!(c),
                None => serde_json::Value::Null,
            },
        );
        e
    }

    #[test]
    fn cage_deny_is_toasted_with_capability_and_command() {
        let env = verdict("deny", Some("git.push"), "git push origin main");
        let line = notification_for(&env).expect("deny toast");
        assert!(line.contains("DENIED"));
        assert!(line.contains("git.push"));
        assert!(line.contains("git push origin main"));
    }

    #[test]
    fn cage_default_deny_without_capability_still_toasts() {
        // A deny is always worth surfacing even when it fell through to
        // the default verdict (no named capability).
        let env = verdict("deny", None, "curl evil.example");
        let line = notification_for(&env).expect("deny toast");
        assert!(line.contains("DENIED"));
        assert!(line.contains("curl evil.example"));
    }

    #[test]
    fn cage_capability_allow_is_toasted() {
        let env = verdict("allow", Some("git.commit"), "git commit -m wip");
        let line = notification_for(&env).expect("allow toast");
        assert!(line.contains("allow git.commit"));
        assert!(line.contains("git commit -m wip"));
    }

    #[test]
    fn cage_default_allow_is_silent() {
        // Default-allow traffic (capability null) is the flood we keep
        // journal-only — only policy-matched allows reach the prompt.
        let env = verdict("allow", None, "git config --get user.email");
        assert!(notification_for(&env).is_none());
    }

    #[test]
    fn permission_request_calls_out_risk() {
        let mut env = hook("PermissionRequest");
        env.action = Some("git push --force".into());
        env.risk = Some("high".into());
        let line = notification_for(&env).expect("line");
        assert!(line.contains("approval needed"));
        assert!(line.contains("git push --force"));
        assert!(line.contains("high"));
    }

    #[test]
    fn operator_drift_renders_prompt_notification() {
        let mut env = hook("operator.drift_detected");
        env.source = Some("orkia-operator".into());
        env.message = Some("write target 'orkia-private/x' is outside allowed_paths".into());
        env.extra
            .insert("kind".into(), serde_json::json!("hard_violation"));
        env.extra
            .insert("severity".into(), serde_json::json!("warning"));
        let line = notification_for(&env).expect("operator toast");
        assert!(line.contains("operator drift"));
        assert!(line.contains("hard_violation"));
        assert!(line.contains("outside allowed_paths"));
    }

    #[test]
    fn operator_cross_session_renders_prompt_notification() {
        let mut env = hook("operator.cross_session_conflict");
        env.source = Some("orkia-operator".into());
        env.message = Some("write target 'src/auth.rs' intersects watch_paths for job 2".into());
        let line = notification_for(&env).expect("operator toast");
        assert!(line.contains("operator cross-session"));
        assert!(line.contains("watch_paths"));
    }

    #[test]
    fn stop_and_subagent_stop_are_silent() {
        // `Stop` fires after every claude turn, not at session end —
        // surfacing it as "✓ completed" was a false positive that
        // appeared right after every prompt the user typed. The job
        // lifecycle is signalled by `JobEvent::Completed` (rendered
        // as `[N] done (exit N)`); hooks stay journal-only.
        assert!(notification_for(&hook("Stop")).is_none());
        assert!(notification_for(&hook("SubagentStop")).is_none());
    }

    #[test]
    fn session_start_is_silent() {
        assert!(notification_for(&hook("SessionStart")).is_none());
        assert!(notification_for(&hook("UserPromptSubmit")).is_none());
    }

    #[test]
    fn shell_and_seal_envelopes_are_silent() {
        let mut shell = JournalEnvelope::now(EventType::Shell);
        shell.action = Some("ls".into());
        assert!(notification_for(&shell).is_none());

        let mut seal = JournalEnvelope::now(EventType::Seal);
        seal.event = Some("approval.resolved".into());
        assert!(notification_for(&seal).is_none());
    }

    #[test]
    fn lifecycle_completed_is_silent_in_toasts() {
        // Lifecycle completions are rendered by the shell-mode
        // `JobEvent` printer; the journal toast layer must not also
        // emit a line (was a duplicate `[1] done` + `✗ failed`).
        let mut env = JournalEnvelope::now(EventType::Lifecycle);
        env.event = Some("completed".into());
        env.job_id = Some(2);
        env.exit_code = Some(7);
        assert!(notification_for(&env).is_none());
    }

    #[test]
    fn lifecycle_spawn_is_silent() {
        let mut env = JournalEnvelope::now(EventType::Lifecycle);
        env.event = Some("spawn".into());
        assert!(notification_for(&env).is_none());
    }

    #[test]
    fn tell_truncates_long_messages() {
        let mut env = JournalEnvelope::now(EventType::Tell);
        env.job_id = Some(1);
        env.message = Some("a".repeat(200));
        let line = notification_for(&env).expect("line");
        assert!(line.contains('…'));
        assert!(line.chars().count() < 200);
    }

    #[test]
    fn tell_from_orkia_is_silent_in_toasts() {
        // Internal `Tell` envelopes (handle_tell / emit_injection /
        // deliver_to_existing_agent) get their own live toast at the
        // call site; the journal-toast layer must not render them
        // again or the user sees `→ msg` minutes later.
        let mut env = JournalEnvelope::now(EventType::Tell);
        env.job_id = Some(1);
        env.source = Some("orkia".into());
        env.message = Some("say hellooo".into());
        assert!(notification_for(&env).is_none());
    }

    #[test]
    fn tell_from_external_source_still_renders() {
        // A Tell coming from outside orkia (future cross-shell sync,
        // etc.) is the path the `→` toast was designed for; keep it.
        let mut env = JournalEnvelope::now(EventType::Tell);
        env.job_id = Some(1);
        env.source = Some("peer-shell".into());
        env.message = Some("ping".into());
        let line = notification_for(&env).expect("line");
        assert!(line.contains('→'));
        assert!(line.contains("ping"));
    }

    #[test]
    fn tag_falls_back_when_no_agent() {
        // Use `Notification` rather than `Stop` so we still get a
        // visible line (Stop / SubagentStop are silent in toasts).
        let mut env = hook("Notification");
        env.agent = None;
        env.message = Some("hi".into());
        let line = notification_for(&env).expect("line");
        assert!(line.contains("[job 1]"));
    }

    #[test]
    fn foreign_hook_without_job_id_is_silent() {
        // External claude session (assistant CLI, another shell) with
        // `orkia bridge` wired into hooks but no `ORKIA_JOB_ID` env →
        // job_id is None. Its tool calls must not toast inside the
        // orkia REPL — they belong to a process the user didn't
        // spawn from here. The envelope still goes through the
        // journal store + SEAL; only the toast path is gated.
        let mut env = JournalEnvelope::now(EventType::Hook);
        env.event = Some("PreToolUse".into());
        env.tool = Some("Bash".into());
        env.target = Some("cargo test".into());
        // job_id intentionally left None.
        assert!(notification_for(&env).is_none());
    }
}

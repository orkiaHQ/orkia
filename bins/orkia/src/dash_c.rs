// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use orkia_shell::engine::ShellEngine;
use orkia_shell::{ShellConfig, StdoutRenderer};
use orkia_shell_types::journal::types::{EventType, JournalEnvelope};
use serde_json::json;
use std::path::PathBuf;
use std::time::Instant;

use crate::repl_helpers::{ReplWiring, build_capability_wiring, build_repl};

/// Run a single `-c` command, source `.bashrc`/`.profile`/`.orkiarc`
/// per config for parity with interactive sessions, then exit with
/// brush's reported exit code. Non-fatal: rc errors go to stderr but
/// don't shadow the command's exit code. This is the path `ssh host
/// 'cmd'` and cron rely on, so it must terminate cleanly.
pub(crate) async fn run_dash_c(
    cmd: &str,
    config: &ShellConfig,
    login: bool,
    timeout_secs: Option<u64>,
    audit: bool,
    detach: bool,
) -> i32 {
    if detach {
        // Capture the caller's cwd so the daemon-spawned runtime — and the
        // agent it drives — runs in the user's directory, not the daemon's.
        // Without this the agent inherits the daemon's cwd (whatever directory
        // the daemon was first started in), so a relative path like
        // `@sage review src/auth.rs` resolves against the wrong directory and
        // the agent's hooks/transcript land under the wrong project. The daemon
        // already honors `Spawn.working_dir`; the `--detach` client just never
        // set it.
        let mut req = crate::pty_daemon::SpawnDetachedRequest::new(cmd);
        req.working_dir = std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string());
        return crate::pty_daemon::spawn_detached_request(req, config);
    }

    // Commands that the shell engine can't classify on its own —
    // `@agent ...` agent dispatch and orkia builtins — need the full
    // REPL pipeline (classifier → router → dispatch → SEAL). Anything
    // else stays on the legacy bare-brush path so cron/ssh footprint
    // is unchanged for plain shell commands.
    if needs_repl_pipeline(cmd) {
        return apply_timeout(
            timeout_secs,
            run_dash_c_via_repl(cmd, config.clone(), login),
        )
        .await;
    }

    let opts = orkia_shell::engine::ShellEngineOptions {
        load_bashrc: config.load_bashrc.unwrap_or(true),
        load_profile: config.load_profile.unwrap_or(true),
        login,
    };
    let mut engine = match ShellEngine::new_with_options(opts).await {
        Ok(e) => e,
        Err(err) => {
            eprintln!("orkia: failed to start shell engine: {err}");
            return 127;
        }
    };
    for (path, err) in engine.source_default_rc(opts).await {
        eprintln!("orkia: warning: {}: {err}", path.display());
    }
    if let Some(home) = std::env::var_os("HOME") {
        let path = PathBuf::from(home).join(".orkiarc");
        if let Err(err) = engine.source_if_exists(&path).await {
            eprintln!("orkia: warning: .orkiarc: {err}");
        }
    }
    let started = Instant::now();
    let code = apply_timeout(timeout_secs, async move {
        match engine.execute(cmd).await {
            Ok(r) => r.exit_code as i32,
            Err(err) => {
                eprintln!("orkia: {err}");
                127
            }
        }
    })
    .await;
    if audit && let Err(err) = audit_shell_command(config, cmd, code, started.elapsed()) {
        eprintln!("orkia: audit write failed: {err}");
        return 126;
    }
    code
}

/// Wrap a future in a `--timeout` cap when one is set. Exit 124 is
/// the GNU `timeout(1)` convention for "command timed out" and is
/// what cron-aware scripts already check for.
pub(crate) async fn apply_timeout<F>(timeout_secs: Option<u64>, fut: F) -> i32
where
    F: std::future::Future<Output = i32>,
{
    match timeout_secs {
        Some(secs) => match tokio::time::timeout(std::time::Duration::from_secs(secs), fut).await {
            Ok(code) => code,
            Err(_) => {
                eprintln!("orkia: scheduled run exceeded {secs}s timeout");
                124
            }
        },
        None => fut.await,
    }
}

/// Heuristic for whether a `-c` payload needs the REPL pipeline. This
/// stays quote-aware so data containing `@` (emails, strings passed to
/// printf) keeps bash-compatible brush behavior, while any unquoted
/// command position that starts with `@agent` wakes governance.
pub(crate) fn needs_repl_pipeline(cmd: &str) -> bool {
    let trimmed = cmd.trim_start();
    starts_with_orkia_builtin(trimmed) || has_unquoted_agent_command(trimmed)
}

async fn run_dash_c_via_repl(cmd: &str, config: ShellConfig, login: bool) -> i32 {
    let (classifier, handle, resolver, auth) = build_capability_wiring();
    let mut repl = build_repl(
        StdoutRenderer::new(),
        ReplWiring {
            classifier,
            config,
            login,
            resolver,
            handle,
            auth,
            tui_factory: None,
        },
    );
    match repl.run_one_command(cmd.to_string()).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("orkia: {err}");
            127
        }
    }
}

fn starts_with_orkia_builtin(trimmed: &str) -> bool {
    trimmed == "orkia" || trimmed.starts_with("orkia ")
}

fn has_unquoted_agent_command(cmd: &str) -> bool {
    let mut single = false;
    let mut double = false;
    let mut escaped = false;
    let mut command_position = true;

    for (idx, ch) in cmd.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && !single {
            escaped = true;
            continue;
        }
        match ch {
            '\'' if !double => {
                single = !single;
                continue;
            }
            '"' if !single => {
                double = !double;
                continue;
            }
            _ => {}
        }
        if single || double {
            continue;
        }
        if ch.is_whitespace() {
            continue;
        }
        if is_command_separator(ch) {
            command_position = true;
            continue;
        }
        if command_position && ch == '@' {
            return is_agent_name_start(cmd[idx + ch.len_utf8()..].chars().next());
        }
        command_position = false;
    }

    false
}

fn is_command_separator(ch: char) -> bool {
    matches!(ch, '|' | ';' | '&' | '(')
}

fn is_agent_name_start(ch: Option<char>) -> bool {
    ch.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn audit_shell_command(
    config: &ShellConfig,
    cmd: &str,
    exit_code: i32,
    duration: std::time::Duration,
) -> Result<(), String> {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| String::new());
    let start_detail = json!({
        "command": cmd,
        "cwd": cwd,
        "origin": audit_origin(),
    });
    let complete_detail = json!({
        "command": cmd,
        "cwd": std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| String::new()),
        "exit_code": exit_code,
        "duration_ms": duration.as_millis() as u64,
        "origin": audit_origin(),
    });
    let mut manager = orkia_shell::SealManager::new(config.data_dir.clone());
    manager
        .seal_workspace("shell.start", start_detail.clone())
        .map_err(|e| e.to_string())?;
    manager
        .seal_workspace("shell.complete", complete_detail.clone())
        .map_err(|e| e.to_string())?;

    let mut journal = orkia_shell::journal::store::JournalStore::new(&config.data_dir);
    journal.append(&shell_audit_envelope("shell.start", None, &start_detail));
    journal.append(&shell_audit_envelope(
        "shell.complete",
        Some(exit_code),
        &complete_detail,
    ));
    Ok(())
}

fn shell_audit_envelope(
    event: &str,
    exit_code: Option<i32>,
    detail: &serde_json::Value,
) -> JournalEnvelope {
    let mut env = JournalEnvelope::now(EventType::Shell);
    env.source = Some("orkia".to_string());
    env.event = Some(event.to_string());
    env.exit_code = exit_code;
    if let Some(obj) = detail.as_object() {
        for (key, value) in obj {
            env.extra.insert(key.clone(), value.clone());
        }
    }
    env
}

fn audit_origin() -> &'static str {
    match std::env::var("ORKIA_SCHEDULED").as_deref() {
        Ok("1") => "scheduled",
        _ => "manual",
    }
}

#[cfg(test)]
mod tests {
    use super::needs_repl_pipeline;

    #[test]
    fn routes_direct_agent() {
        assert!(needs_repl_pipeline("@faye task"));
    }

    #[test]
    fn routes_shell_to_agent_pipe() {
        assert!(needs_repl_pipeline("git diff | @sage audit"));
    }

    #[test]
    fn routes_agent_to_shell_sink() {
        assert!(needs_repl_pipeline("@sage review | tee out.txt"));
    }

    #[test]
    fn routes_agent_pipeline() {
        assert!(needs_repl_pipeline("@a | @b"));
    }

    #[test]
    fn ignores_email_in_double_quotes() {
        assert!(!needs_repl_pipeline("echo \"email@domain.com\""));
    }

    #[test]
    fn ignores_quoted_at_payload() {
        assert!(!needs_repl_pipeline("printf '@not-agent\\n'"));
    }

    #[test]
    fn routes_orkia_builtin() {
        assert!(needs_repl_pipeline("orkia ps"));
    }

    #[test]
    fn keeps_plain_shell_on_brush() {
        assert!(!needs_repl_pipeline("ls -la"));
    }

    #[test]
    fn dash_c_is_posix_first() {
        // system's, byte-for-byte — only the explicit `orkia ` namespace
        // reaches the builtin pipeline.
        assert!(!needs_repl_pipeline("ps"));
        assert!(!needs_repl_pipeline("ps aux"));
        assert!(!needs_repl_pipeline("whoami"));
        assert!(needs_repl_pipeline("orkia whoami"));
    }
}

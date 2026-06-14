// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

/// Canonical agent attachment preset. Builds the `JobConfig` that
/// the REPL passes to [`crate::job::JobController::spawn`] for both
/// ad-hoc `@agent` invocations and `rfc delegate` flows. Attachment
/// must record `agent.spawn` before downstream events).
pub(crate) fn build_agent_job_config(input: AgentJobConfigInput<'_>) -> crate::job::JobConfig<'_> {
    use crate::job::Attachment;

    // The job's single provider identity: an explicit `[hooks] provider`
    // wins over the command basename (warn on mismatch). Consumed by the
    // hook installer and the context spawn plan alike.
    let provider = orkia_shell_types::ProviderId::derive(input.hooks_provider, input.cmd);

    let mut attachments: Vec<Attachment> = Vec::with_capacity(6);
    // Install provider hooks whenever the RESOLVED provider supports hook
    // capture — `provider` is already derived from the runtime command
    // (so `command = "claude"` resolves to Claude even without a redundant
    // `[hooks] provider`). Hooks are the whole approval + SEAL +
    // final-response-capture story (engineering principle #5): a
    // claude/codex/gemini runtime must never spawn without them, else its
    // turns run but the Stop hook never fires and the final response is
    // never captured. A bare shell command resolves to Generic (no
    // capture capability) and is correctly skipped.
    if provider.capabilities().hooks_capture {
        attachments.push(Attachment::Hooks {
            // The `orkia-sh hook` PreToolUse entry is the **macOS** per-command
            // Linux the gate is the sole-shell `-c` shim, and the host orkia-sh
            // path baked into the hook wouldn't resolve inside the rootfs — so
            // we skip it there. Off the cage entirely the hook has no policy.
            // Capability-gated: only a provider that can cooperatively deny
            // (Claude today) gets the mediation entry at all.
            mediate: input.cage_wrapper.is_some()
                && cfg!(target_os = "macos")
                && provider.capabilities().cooperative_deny,
        });
    }
    if let Some(context) = input.agent_context {
        attachments.push(Attachment::AgentContext { context });
    }
    attachments.push(Attachment::Osc133Listener);
    attachments.push(Attachment::SealChain {
        project: input.project,
    });
    attachments.push(Attachment::StateMachine {
        pending_body: input.pending_body,
    });
    attachments.push(Attachment::InjectionExecutor);

    crate::job::JobConfig {
        command: input.cmd,
        provider,
        args: input.args,
        label: format!("{} ({})", input.agent_name, input.cmd),
        env: input.extra_env,
        working_dir: input.working_dir,
        stdin: input.stdin,
        process_group: orkia_shell_types::ProcessGroupMode::NewSession,
        attachments,
        cage_wrapper: input.cage_wrapper,
    }
}

/// The `$HOME` an agent is spawned with — where its trust config lives
/// (orkia exports the same `HOME` to the agent). `None` if unset, in
/// which case no provider-config pre-trust is possible.
pub(crate) fn trust_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

pub(crate) fn is_valid_agent_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// non-blocking `try_lock` (sorted, deduped). Returns empty if the brush
/// mutex is momentarily held — pre-prompt this is uncontended, so it
/// effectively always succeeds; never blocks the prompt path.
pub(crate) fn read_alias_names(brush: &Arc<tokio::sync::Mutex<BrushSession>>) -> Vec<String> {
    let Ok(guard) = brush.try_lock() else {
        return Vec::new();
    };
    let mut names = guard.alias_names();
    names.sort();
    names.dedup();
    names
}

/// directory. Comparing this between prompts (cold) detects a binary added to
/// an existing PATH dir (its dir's mtime bumps) as well as a changed `$PATH`.
pub(crate) fn current_path_mtimes() -> Vec<(std::path::PathBuf, Option<std::time::SystemTime>)> {
    let mut out = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let mtime = std::fs::metadata(&dir).ok().and_then(|m| m.modified().ok());
            out.push((dir, mtime));
        }
    }
    out
}

pub(crate) fn parse_background(trimmed: String) -> (String, bool) {
    if trimmed.ends_with(" &") {
        (trimmed[..trimmed.len() - 2].trim().to_string(), true)
    } else {
        (trimmed, false)
    }
}

/// Whether a (trimmed) REPL line is a top-level quit command — bare
/// `exit` or `quit`, with an optional ignored argument (`exit 0`). A
/// forced-shell `!exit` never matches (its first word is `!exit`), nor
/// does anything that merely contains the word (`echo exit`). See
/// finding #4: these must reliably terminate the shell.
pub(crate) fn is_quit_command(trimmed: &str) -> bool {
    matches!(
        trimmed.split_whitespace().next(),
        Some("exit") | Some("quit")
    )
}

/// Exit code for a quit command, bash-style: `exit N` → `N`, bare
/// non-numeric arg (`exit foo`) → `2`. The OS truncates to the low
/// 8 bits at process-exit time. Caller has already confirmed
/// [`is_quit_command`].
pub(crate) fn quit_exit_code(trimmed: &str, last_status: i32) -> i32 {
    match trimmed.split_whitespace().nth(1) {
        None => last_status,
        Some(arg) => arg.parse::<i32>().unwrap_or(2),
    }
}

/// Pull every `@<agent> [body]` segment out of `line` and synthesize
/// the matching `PipelineStage` vec. Used by the mixed
/// shell-then-multi-agent route in `tick` — we drop any leading
/// non-`@` segment (Solo can't execute it; the Team coordinator
/// handles the shell prefix separately). Returns `None` if fewer
/// than two agent stages are found.
pub(crate) fn synthesize_agent_chain_stages(line: &str) -> Option<Vec<PipelineStage>> {
    let mut stages: Vec<PipelineStage> = Vec::new();
    for raw in line.split('|') {
        let trimmed = raw.trim();
        let Some(rest) = trimmed.strip_prefix('@') else {
            continue;
        };
        let mut it = rest.splitn(2, char::is_whitespace);
        let agent = it.next().unwrap_or("").trim().to_string();
        let body = it.next().unwrap_or("").trim().to_string();
        if agent.is_empty() {
            return None;
        }
        stages.push(PipelineStage { agent, body });
    }
    if stages.len() >= 2 {
        Some(stages)
    } else {
        None
    }
}

/// SHA-256 first 16 hex chars, matching the convention used in
/// `agent_context::short_sha` and `seal::consumer::short_sha`. Local
/// to this module because the call site is only `dispatch_shell_to_agent`.
pub(crate) fn short_sha(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let full = hex::encode(hasher.finalize());
    full.chars().take(16).collect()
}

/// Tokenize a builtin-args string with simple shell-style quoting.
/// Single and double quotes group contents; no escape sequences are handled.
pub(crate) fn tokenize_args(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in input.chars() {
        match (c, quote) {
            ('"', None) => quote = Some('"'),
            ('\'', None) => quote = Some('\''),
            (q, Some(open)) if q == open => quote = None,
            (c, None) if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (c, _) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub(crate) fn build_migration_summary(
    report: &orkia_builtin::migrate_rc::MigrationReport,
) -> Vec<BlockContent> {
    let mut blocks = Vec::new();
    blocks.push(BlockContent::SystemInfo(format!(
        "source: {} ({})",
        report.source_path.display(),
        report.kind.name()
    )));
    if report.counts.migrated > 0 {
        blocks.push(BlockContent::SystemInfo(format!(
            "✓ {} migrated",
            report.counts.migrated
        )));
    }
    if report.counts.translated > 0 {
        blocks.push(BlockContent::SystemInfo(format!(
            "✓ {} translated",
            report.counts.translated
        )));
    }
    if report.counts.comments > 0 {
        blocks.push(BlockContent::SystemInfo(format!(
            "  {} comments preserved",
            report.counts.comments
        )));
    }
    if report.counts.skipped > 0 {
        blocks.push(BlockContent::SystemInfo(format!(
            "⚠ {} skipped:",
            report.counts.skipped
        )));
        for (orig, reason) in &report.skipped {
            blocks.push(BlockContent::SystemInfo(format!(
                "    {} → {reason:?}",
                orig.trim()
            )));
        }
    }
    blocks
}

/// Resolve `$HOME`. We avoid the `home` crate dependency since `$HOME` is
/// what the shell uses anyway — brush already reads it the same way.
pub(crate) fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// Quote a single path/argument for safe interpolation into a brush
/// command line. Uses single-quote wrapping with escaped inner quotes
/// — POSIX-portable and what brush understands natively.
pub(crate) fn shell_escape(s: &str) -> String {
    if !s.contains([
        '\'', ' ', '"', '$', '`', '\\', '*', '?', '!', '&', '|', ';', '(', ')',
    ]) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Find the `orkia-forge-viewer` binary. V0 only checks `$PATH` (via
/// shell semantics) and a sibling `target/debug` next to the running
/// orkia binary, since we ship from a workspace build.
/// Find the most recent `<rfc-slug>-*.seal.jsonl` document for an RFC,
/// or `None` if none exist yet. Used by `orkia rfc seal <slug>` to
/// decide whether to assemble or display the existing file.
pub(crate) fn find_latest_seal_v1_document(
    data_dir: &std::path::Path,
    slug: &str,
) -> Option<std::path::PathBuf> {
    let seal_dir = data_dir.join("seal-v1");
    let entries = std::fs::read_dir(&seal_dir).ok()?;
    let prefix = format!("{slug}-");
    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) if n.starts_with(&prefix) && n.ends_with(".jsonl") => n,
            _ => continue,
        };
        let _ = name;
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        match &best {
            Some((t, _)) if *t >= mtime => {}
            _ => best = Some((mtime, path)),
        }
    }
    best.map(|(_, p)| p)
}

pub(crate) fn locate_viewer_binary() -> Option<std::path::PathBuf> {
    // 1. Adjacent to the current exe — works for `cargo run` and installed bins.
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("orkia-forge-viewer");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    // 2. $PATH lookup.
    if let Some(path) = std::env::var_os("PATH") {
        for entry in std::env::split_paths(&path) {
            let candidate = entry.join("orkia-forge-viewer");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Emit an OSC 133 single-letter marker (A / B / C) to stderr.
/// stderr because the prompt itself renders there and stdout is
/// reserved for command output.
pub(crate) fn emit_osc133(marker: &str) {
    use std::io::Write;
    let mut err = std::io::stderr().lock();
    let _ = write!(err, "\x1b]133;{marker}\x07");
    let _ = err.flush();
}

/// Emit OSC 133 `D;N` — output finished, with exit code.
pub(crate) fn emit_osc133_finished(exit_code: i32) {
    use std::io::Write;
    let mut err = std::io::stderr().lock();
    let _ = write!(err, "\x1b]133;D;{exit_code}\x07");
    let _ = err.flush();
}

/// Render a `DetectorEvent` into the one-line ANSI toast the
/// state-machine worker thread feeds to the `ExternalPrinter`.
/// Returns `None` for events that are side-effect-only (`Closed`).
pub(crate) fn format_detector_event(
    event: &crate::terminal_state::DetectorEvent,
) -> Option<String> {
    use crate::terminal_state::DetectorEvent::*;
    use crate::terminal_state::PromptType;
    match event {
        Attention(att) => {
            let percent = (att.confidence * 100.0).round() as i32;
            let tag = format!("\x1b[90m[job {} {}]\x1b[0m", att.job_id.0, att.agent_name);
            let body = match (&att.prompt_type, &att.pending_body_preview) {
                (PromptType::Password, _) => format!(
                    "\x1b[33m⚠ password prompt — attach %{} to enter\x1b[0m",
                    att.job_id.0
                ),
                (_, Some(pending)) => format!(
                    "\x1b[33m⚠ waiting for input ({percent}%) — pending: \"{pending}\"\x1b[0m",
                ),
                (_, None) => format!(
                    "\x1b[33m⚠ waiting for input ({percent}%): {}\x1b[0m",
                    truncate(&att.last_line, 60)
                ),
            };
            Some(format!("  {tag} {body}"))
        }
        // The decision to inject is silent — the toast waits for the
        // executor's `Delivered` so it reflects the real landing.
        Injected { .. } => None,
        Delivered {
            job_id,
            agent_name,
            body,
        } => {
            let tag = format!("\x1b[90m[job {} {agent_name}]\x1b[0m", job_id.0);
            Some(format!(
                "  {tag} \x1b[36m▸ prompt injected:\x1b[0m \"{}\"",
                truncate(body, 60)
            ))
        }
        Closed { .. } => None,
    }
}

pub(crate) fn parse_resolution_target(args: &[String]) -> Option<JobId> {
    args.first().and_then(|a| a.parse::<u32>().ok()).map(JobId)
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// Detect shell metacharacters that brush's `expand_to_argv`
/// can't represent (it produces one argv per simple command).
/// When present, we route the backgrounded command through
/// `sh -c` so pipelines and chains still work. Conservative: a
/// `|` inside single-quotes is detected too, but the worst case
/// is "this command works correctly via sh -c instead of via
/// direct argv spawn" — same outcome.
pub(crate) fn cmd_contains_shell_operators(cmd: &str) -> bool {
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '|' | ';' | '(' | ')' => return true,
            '&' if chars.peek() == Some(&'&') => return true,
            _ => {}
        }
    }
    false
}

/// Walk `<data_dir>/jobs/*/output.log` and delete any whose
/// mtime is older than 7 days. Best-effort: errors are logged
/// but never propagated. Called once at REPL boot.
pub(crate) fn gc_old_job_logs(data_dir: &std::path::Path) {
    let jobs_dir = data_dir.join("jobs");
    let entries = match std::fs::read_dir(&jobs_dir) {
        Ok(d) => d,
        Err(_) => return, // no jobs/ yet, nothing to GC
    };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 24 * 60 * 60);
    let mut removed = 0;
    for entry in entries.flatten() {
        let log_path = entry.path().join("output.log");
        let Ok(meta) = std::fs::metadata(&log_path) else {
            continue;
        };
        let Ok(mtime) = meta.modified() else { continue };
        if mtime < cutoff && std::fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        tracing::info!(removed, "seal: gc'd stale job log dirs");
    }
}

#[cfg(test)]
mod quit_command_tests {
    use super::{is_quit_command, quit_exit_code};

    #[test]
    fn exit_code_propagates_bash_style() {
        assert_eq!(quit_exit_code("exit", 0), 0, "bare exit → last $?");
        assert_eq!(quit_exit_code("exit", 2), 2, "bare exit propagates $?");
        assert_eq!(quit_exit_code("quit", 1), 1, "bare quit propagates $?");
        assert_eq!(quit_exit_code("exit 7", 0), 7, "exit N → N");
        assert_eq!(quit_exit_code("exit 0", 5), 0, "explicit N beats $?");
        assert_eq!(quit_exit_code("quit 42", 0), 42);
        assert_eq!(
            quit_exit_code("exit -1", 0),
            -1,
            "OS truncates to 255 at exit"
        );
        assert_eq!(
            quit_exit_code("exit foo", 0),
            2,
            "non-numeric arg → 2 (bash)"
        );
    }

    #[test]
    fn recognizes_bare_quit_commands() {
        for s in ["exit", "quit", "exit 0", "quit now", "exit\t"] {
            assert!(is_quit_command(s.trim()), "{s:?} should be a quit command");
        }
    }

    #[test]
    fn does_not_overmatch() {
        for s in [
            "",
            "exitx",
            "quitter",
            "!exit",
            "echo exit",
            "cd exit",
            "exited",
        ] {
            assert!(!is_quit_command(s.trim()), "{s:?} must NOT quit the shell");
        }
    }
}

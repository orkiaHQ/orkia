// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Two entry modes share one decision core (`core::decide`):
//!   makes this the only shell, so every shell-out reaches it. allow → exec the
//!   preserved real shell; deny/ask → refuse, non-zero, no exec.
//! - **hook** (`orkia-sh hook`) — macOS coverage: a Claude PreToolUse hook (the
//!   only supported gate — the Bash tool's shell binary is not overridable).
//!   Best-effort + cooperative; the kernel guarantee on macOS is the Seatbelt
//!
//! Re-entrance clearance: an allowed command legitimately spawns subshells
//! (git hooks, pagers, npm); `ORKIA_SH_CLEARED` passes those straight through.
//!
//! Fail-closed everywhere (CLAUDE.md #8): missing/unreadable policy → deny;
//! unparseable agent envelope → deny; audit-write failure on an `allow` → deny.

mod core;
mod decide;
mod hook;
mod verdict;

use std::os::unix::process::CommandExt;
use std::process::Command;

use anyhow::{Context, Result};
use orkia_shell_types::Verdict;

use crate::core::Decision;

/// Set in the child env when we exec the real shell for an allowed command, so
/// the subshells it spawns pass through without re-evaluation.
const CLEARED_ENV: &str = "ORKIA_SH_CLEARED";
/// The preserved real shell to exec for allowed commands / passthrough.
const REAL_SHELL_ENV: &str = "ORKIA_SH_REAL";
const DEFAULT_REAL_SHELL: &str = "/bin/bash";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Hook entry mode (macOS PreToolUse mediation): reads the hook JSON on stdin.
    if args.first().map(String::as_str) == Some("hook") {
        return hook::run();
    }

    // Not a `-c` invocation (interactive/login/script). V1 mediates only the
    // `-c` form; pass other forms through so the agent's shell still works.
    let Some(command) = dash_c_command(&args) else {
        return exec_real_shell(&args, false);
    };

    // Already cleared: a descendant subshell of an allowed command.
    if std::env::var_os(CLEARED_ENV).is_some() {
        return exec_real_shell(&args, false);
    }

    mediate(&args, &command)
}

/// Evaluate the top-level command and act on the decision (shell-shim mode).
///
/// The three tiers are separate arms (not an `Ask | Deny` merge): `Allow` execs,
/// `Deny` and `Ask` both refuse in V1, but a future trust layer would widen the
/// `Ask` arm alone — `Deny` is terminal and never reachable from trust.
fn mediate(args: &[String], raw_command: &str) -> Result<()> {
    match core::decide(raw_command) {
        Decision::Allow {
            command,
            capability,
            rule,
        } => {
            // An allow proceeds only if the decision was durably recorded
            // (CLAUDE.md #8: audit-write failure aborts the call).
            verdict::emit(
                &command,
                Verdict::Allow,
                capability.as_deref(),
                rule.as_deref(),
            )
            .context("refusing to run: cage.verdict audit write failed (fail-closed)")?;
            // Run the *original* args (the agent's full wrapper), not the
            // unwrapped string — only the *match* used the unwrapped form.
            //
            // Linux: supervise the command (fork+wait) so we observe its exit and
            // emit a `command.outcome` — the result-quality trust signal. macOS /
            // other unix have no sole-shell guarantee here; outcome there comes
            // from the PostToolUse hook, so we keep the transparent `exec`.
            #[cfg(target_os = "linux")]
            {
                run_supervised(args, capability.as_deref())
            }
            #[cfg(not(target_os = "linux"))]
            {
                exec_real_shell(args, true)
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
            deny(&command, capability.as_deref(), forced_reason)
        }
        // V1: ask is recorded as `ask` but enforced as deny. Its own arm so a
        // future trust layer attaches here without touching the terminal Deny.
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
            deny(&command, capability.as_deref(), None)
        }
    }
}

/// Print the standard refusal and exit non-zero **without** exec'ing.
fn deny(command: &str, capability: Option<&str>, forced: Option<&'static str>) -> ! {
    match (capability, forced) {
        (Some(cap), _) => eprintln!("DENIED by orkia-cage (capability: {cap}): {command}"),
        (None, Some(reason)) => eprintln!("DENIED by orkia-cage ({reason}): {command}"),
        (None, None) => eprintln!("DENIED by orkia-cage (no matching allow): {command}"),
    }
    std::process::exit(126) // 126: command found but not permitted
}

/// Replace this process with the preserved real shell, forwarding the original
/// argv. When `set_cleared` is true, mark the child so its subshells pass
/// through. `exec` only returns on failure.
fn exec_real_shell(args: &[String], set_cleared: bool) -> Result<()> {
    let real = real_shell();
    let mut cmd = Command::new(&real);
    cmd.args(args);
    if set_cleared {
        cmd.env(CLEARED_ENV, "1");
    }
    let err = cmd.exec();
    Err(err).with_context(|| format!("failed to exec real shell `{real}`"))
}

/// The preserved real shell to run for allowed commands.
fn real_shell() -> String {
    std::env::var(REAL_SHELL_ENV).unwrap_or_else(|_| DEFAULT_REAL_SHELL.to_string())
}

/// Linux: run an allowed command under supervision instead of `exec`-replacing
/// ourselves, so we observe its exit status and emit a `command.outcome` (the
/// result-quality trust signal). The child runs the *real shell* with
/// `ORKIA_SH_CLEARED=1`, so its own subshells pass straight through — exactly one
/// supervised level per top-level command, never a cascade. We then exit with the
/// child's code, preserving the transparent-shell contract for the caller.
///
/// `command.outcome` is best-effort (the command already ran); only the exit-code
/// propagation is load-bearing.
#[cfg(target_os = "linux")]
fn run_supervised(args: &[String], capability: Option<&str>) -> Result<()> {
    let real = real_shell();
    let mut cmd = Command::new(&real);
    cmd.args(args).env(CLEARED_ENV, "1");
    let child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn real shell `{real}`"))?;
    let pid = nix::unistd::Pid::from_raw(child.id() as i32);
    // We reap the child ourselves via `waitpid`; forget the std handle so its
    // `Drop` does not also try to wait (double-reap → ECHILD).
    std::mem::forget(child);

    let code = supervise(pid)?;
    verdict::emit_outcome(capability, code == 0, Some(code as i64));
    std::process::exit(code);
}

/// Parent-side supervisor (mirrors `orkia-cage`'s `linux_sb::supervise`): forward
/// terminal/job-control signals to the child so Ctrl-C / Ctrl-\ / resize reach
/// the real command instead of dying here, and return the child's exit code
/// (`128 + signo` if it was killed by a signal).
///
/// NB (CLAUDE.md #6): interactive Ctrl-C / Ctrl-Z / SIGWINCH on this per-command
/// path must still be validated against a real agent on a PTY before it is
/// trusted in production — same outstanding validation the cage supervisor carries.
#[cfg(target_os = "linux")]
fn supervise(child: nix::unistd::Pid) -> Result<i32> {
    use nix::sys::signal::{SigSet, Signal, kill};
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};

    let forwarded = [
        Signal::SIGINT,
        Signal::SIGTERM,
        Signal::SIGQUIT,
        Signal::SIGHUP,
        Signal::SIGWINCH,
    ];
    // Block the forwarded set + SIGCHLD in the parent (after spawn, so the child
    // started with a normal mask) and consume them via sigwait.
    let mut mask = SigSet::empty();
    for s in forwarded {
        mask.add(s);
    }
    mask.add(Signal::SIGCHLD);
    mask.thread_block()
        .context("block signals for supervisor")?;
    loop {
        let sig = mask.wait().context("sigwait")?;
        if sig == Signal::SIGCHLD {
            match waitpid(child, Some(WaitPidFlag::WNOHANG)).context("waitpid child")? {
                WaitStatus::Exited(_, code) => return Ok(code),
                WaitStatus::Signaled(_, s, _) => return Ok(128 + s as i32),
                _ => continue, // stopped/continued — keep supervising
            }
        } else {
            let _ = kill(child, sig); // forward to the command
        }
    }
}

/// The command string operand of a `-c` invocation, if any.
///
/// Agents do not all spell `-c` the same way:
/// - Codex clusters the flags: `bash -lc "<cmd>"` (handled by [`is_command_flag`]
///   matching any short cluster ending in `c`);
/// - **Claude Code** (verified 2026-06-04, v2.1.162, real capture) invokes
///   `bash -c -l "<cmd>"` — the login flag sits **between** `-c` and the command
///   string. Returning `args[pos + 1]` blindly captured the `-l`, not the
///   command, so the `eval '…'` envelope was never unwrapped and the policy
///   matched against `"-l"` — a **fail-open hole** for Claude under a
///   default-allow policy (a denied command in the envelope fell through to the
///   default). See `qa/linux/agent-shells.md`.
///
/// bash treats every leading `-…` after `-c` as a further option; the command
/// string is the **first non-option operand**. So skip option-looking tokens
/// rather than taking the immediate next arg.
fn dash_c_command(args: &[String]) -> Option<String> {
    let pos = args.iter().position(|a| is_command_flag(a))?;
    args[pos + 1..]
        .iter()
        .find(|a| !is_option_token(a))
        .cloned()
}

/// A token bash parses as an option (so *not* the `-c` command string): any
/// `-…` of length ≥ 2 (short cluster like `-l`/`-i`, or long option like
/// `--norc`). The `-c` command string is the first operand that is not one —
/// a real command never starts with `-` (that would need a `--` separator).
fn is_option_token(arg: &str) -> bool {
    arg.starts_with('-') && arg.len() > 1
}

/// True for a short-flag cluster that requests command-string mode: a single
/// `-` followed by one or more ASCII-lowercase letters and ending in `c`.
fn is_command_flag(arg: &str) -> bool {
    let Some(flags) = arg.strip_prefix('-') else {
        return false;
    };
    !flags.is_empty()
        && !flags.starts_with('-') // exclude long options like `--command`
        && flags.ends_with('c')
        && flags.bytes().all(|b| b.is_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dash_c_extracts_the_command() {
        let args = vec!["-c".to_string(), "git status".to_string()];
        assert_eq!(dash_c_command(&args), Some("git status".to_string()));
    }

    #[test]
    fn no_dash_c_is_none() {
        let args = vec!["-l".to_string(), "script.sh".to_string()];
        assert_eq!(dash_c_command(&args), None);
    }

    #[test]
    fn dash_c_without_payload_is_none() {
        let args = vec!["-c".to_string()];
        assert_eq!(dash_c_command(&args), None);
    }

    #[test]
    fn clustered_lc_is_a_command_flag() {
        // Codex shells out as `bash -lc "<cmd>"`; the `c` is clustered. Matching
        // only the exact `-c` token would let this pass through unmediated.
        let args = vec!["-lc".to_string(), "git push".to_string()];
        assert_eq!(dash_c_command(&args), Some("git push".to_string()));
    }

    #[test]
    fn clustered_ic_is_a_command_flag() {
        let args = vec!["-ic".to_string(), "rm -rf /".to_string()];
        assert_eq!(dash_c_command(&args), Some("rm -rf /".to_string()));
    }

    #[test]
    fn cluster_without_c_is_not_a_command_flag() {
        assert!(!is_command_flag("-li"));
        assert!(!is_command_flag("-x"));
    }

    #[test]
    fn claude_c_then_l_returns_the_command_not_the_flag() {
        // Claude Code (v2.1.162, real capture) runs `bash -c -l "<cmd>"`: the
        // login flag sits between -c and the command string. We must return the
        // command, not the interposed `-l` — else the eval envelope is never
        // unwrapped and the policy matches against "-l" (fail-open under default
        let args = vec![
            "-c".to_string(),
            "-l".to_string(),
            "eval 'git push' < /dev/null".to_string(),
        ];
        assert_eq!(
            dash_c_command(&args),
            Some("eval 'git push' < /dev/null".to_string())
        );
    }

    #[test]
    fn skips_multiple_options_before_command_string() {
        let args = vec![
            "-c".to_string(),
            "-l".to_string(),
            "-i".to_string(),
            "echo hi".to_string(),
        ];
        assert_eq!(dash_c_command(&args), Some("echo hi".to_string()));
    }

    #[test]
    fn claude_full_envelope_unwraps_after_skipping_l() {
        // End-to-end: the `bash -c -l "<snapshot+eval>"` form must reach
        // `extract_command` and unwrap to the inner command, exactly as the
        // standalone `-c` form does.
        let envelope =
            "shopt -u extglob 2>/dev/null || true && eval 'git push origin main' < /dev/null";
        let args = vec!["-c".to_string(), "-l".to_string(), envelope.to_string()];
        let raw = dash_c_command(&args).expect("command string");
        assert_eq!(
            crate::decide::extract_command(&raw),
            crate::decide::Extracted::Command("git push origin main".to_string())
        );
    }

    #[test]
    fn long_option_is_not_a_command_flag() {
        // A long option that merely ends in `c` must not be mistaken for `-c`.
        assert!(!is_command_flag("--rcfile"));
        assert!(!is_command_flag("--exec"));
    }

    #[test]
    fn dash_c_takes_first_command_flag_operand() {
        let args = vec!["-l".to_string(), "-c".to_string(), "echo hi".to_string()];
        assert_eq!(dash_c_command(&args), Some("echo hi".to_string()));
    }
}

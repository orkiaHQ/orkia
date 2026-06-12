// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

/// Catch SIGINT at the orkia process level so a stray Ctrl-C doesn't
/// terminate the shell, **and** forward it to an attached child when
/// one is active.
///
/// Why this matters:
///   * Real shells (bash, zsh) catch SIGINT for themselves so the user
///     can interrupt a command without killing the shell.
///   * Some terminal-app key bindings (iTerm's "Send Signal: INT" for
///     Ctrl-C) deliver SIGINT directly to orkia instead of writing
///     byte 0x03 to the tty. Without forwarding, our crossterm-based
///     attach pump never sees the keystroke and claude / codex / aider
///     don't get the interrupt.
///
/// tokio's `signal::unix::signal` installs a `sigaction`-based handler
/// (not `SIG_IGN`), so children inherit the default SIGINT disposition
/// after `execve`. That means brush-launched commands still get killed
/// by Ctrl-C as expected.
pub(crate) fn install_sigint_swallow() {
    install_signal_handler(tokio::signal::unix::SignalKind::interrupt(), "SIGINT");
    install_signal_handler(tokio::signal::unix::SignalKind::quit(), "SIGQUIT");
    // SIGTSTP: many terminals (notably iTerm with default signal
    // bindings) deliver Ctrl-Z by `kill(pid, SIGTSTP)` rather than as
    // the byte 0x1a, so termios `ISIG=off` is not enough to surface it
    // as a keystroke. Catch it here and turn it into a detach request
    // when an attach is active, otherwise swallow it so orkia itself
    // is not suspended.
    install_signal_handler(
        tokio::signal::unix::SignalKind::from_raw(libc::SIGTSTP),
        "SIGTSTP",
    );
}

/// Generic per-signal handler installer. Behaviour depends on signal
/// kind and on whether an attach is currently active:
///
/// * `SIGINT` while attached → forward to the child (Ctrl-C UX).
/// * `SIGINT` not attached  → swallow (user pressed Ctrl-C at the
///   shell prompt; rustyline will already have handled the byte case).
/// * `SIGQUIT` while attached → request detach (Ctrl-\ UX). This
///   matches what most TUIs expect when the user wants out of a
///   sub-session without killing it.
/// * `SIGQUIT` not attached  → swallow.
///
/// Both signals MUST be caught or the process dies (default
/// disposition: SIGINT → terminate, SIGQUIT → core dump). iTerm and
/// Terminal.app profiles that map Ctrl-C / Ctrl-\ to "Send Signal"
/// rather than "Send Hex" deliver these directly to orkia, bypassing
/// our crossterm-based raw-mode reader entirely.
pub(crate) fn install_signal_handler(kind: tokio::signal::unix::SignalKind, name: &'static str) {
    use tokio::signal::unix::signal;
    let Ok(mut stream) = signal(kind) else {
        tracing::warn!(signal = name, "orkia: failed to register handler");
        return;
    };
    let raw = kind.as_raw_value();
    tokio::spawn(async move {
        loop {
            if stream.recv().await.is_none() {
                break;
            }
            match orkia_shell::job::foreground::attached_pid() {
                Some(pid) => match raw {
                    n if n == libc::SIGQUIT || n == libc::SIGTSTP => {
                        // User wants out of attach. Don't forward the
                        // signal — that would kill claude. Instead
                        // tell the attach loop to detach normally.
                        orkia_shell::job::foreground::request_detach();
                        tracing::debug!(
                            pid,
                            "orkia: {name} while attached — requesting detach",
                            name = name,
                        );
                    }
                    _ => {
                        // SIGINT or other: find the actual live
                        // descendant. Agents like `claude` / `codex` /
                        // `aider` fork a successor and let the original
                        // wrapper exit, so `kill(pid, …)` returns ESRCH
                        // (errno 3). Walk the process tree to discover
                        // the live PID and signal that.
                        let target_pid = find_child_pid(pid as u32).unwrap_or(pid as u32);
                        // SAFETY: `libc::kill` is FFI-safe with no
                        // memory invariants; `target_pid` is a non-negative
                        // u32 fitted to pid_t. We tolerate ESRCH/EPERM via
                        // the rc check below.
                        let rc = unsafe { libc::kill(target_pid as libc::pid_t, raw) };
                        let errno = if rc != 0 {
                            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
                        } else {
                            0
                        };
                        tracing::debug!(
                            original_pid = pid,
                            target_pid,
                            rc,
                            errno,
                            "orkia: {name} while attached — forwarded to descendant",
                            name = name,
                        );
                    }
                },
                None => {
                    tracing::debug!("orkia: swallowed {name} at shell level", name = name);
                }
            }
        }
    });
}

/// Resolve the live PID for the attached job. Agents like `claude` fork
/// a successor and let the parent exit; the original PID then returns
/// ESRCH. We walk one or two levels of descendants to find a live one.
///
/// Returns the original PID if it is still alive (the common case for
/// `bash`, `vim`, etc.), or the first live descendant found via
/// `pgrep -P` (macOS) or `/proc/<pid>/task/<pid>/children` (Linux).
/// Returns `None` only when neither the parent nor any descendant is
/// alive — caller falls back to the original PID so `kill` still emits
/// a useful errno.
pub(crate) fn find_child_pid(original_pid: u32) -> Option<u32> {
    if process_alive(original_pid) {
        return Some(original_pid);
    }
    #[cfg(target_os = "macos")]
    {
        find_descendants_macos(original_pid)
    }
    #[cfg(target_os = "linux")]
    {
        find_descendants_linux(original_pid)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

pub(crate) fn process_alive(pid: u32) -> bool {
    // SAFETY: `libc::kill` is FFI-safe; signal 0 is documented as a
    // no-side-effect existence check that only inspects permissions
    // and whether the pid resolves.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(target_os = "macos")]
pub(crate) fn find_descendants_macos(parent_pid: u32) -> Option<u32> {
    use std::process::Command;
    let output = Command::new("pgrep")
        .args(["-P", &parent_pid.to_string()])
        .output()
        .ok()?;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(child) = line.trim().parse::<u32>() else {
            continue;
        };
        if process_alive(child) {
            return Some(child);
        }
        // Recurse one level — claude's intermediate may itself be gone.
        if let Some(grand) = find_descendants_macos(child) {
            return Some(grand);
        }
    }
    None
}

#[cfg(target_os = "linux")]
pub(crate) fn find_descendants_linux(parent_pid: u32) -> Option<u32> {
    let path = format!("/proc/{parent_pid}/task/{parent_pid}/children");
    let contents = std::fs::read_to_string(&path).ok()?;
    for tok in contents.split_ascii_whitespace() {
        let Ok(child) = tok.parse::<u32>() else {
            continue;
        };
        if process_alive(child) {
            return Some(child);
        }
        if let Some(grand) = find_descendants_linux(child) {
            return Some(grand);
        }
    }
    None
}

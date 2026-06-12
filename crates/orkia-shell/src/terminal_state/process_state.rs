// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! OS-level introspection of an agent's leaf process: is it sleeping
//! inside a `read(2)` / `poll(2)` / `select(2)` syscall, or is it
//! actively scheduled?
//!
//! Used by the detector as one of three signals. The score is
//! bounded in `[0.0, 1.0]`; the detector treats `>= 0.7` as
//! "blocked", `>= 0.9` as "blocked on tty input specifically".
//!
//! The leaf walk matters: agents like claude fork a successor and
//! let the original wrapper exit. The wrapper PID returns ESRCH; the
//! live child is one or two `pgrep -P` hops down. We always walk to
//! the leaf before probing state.

#[cfg(target_os = "linux")]
use std::fs;

// `Command` is only used by the macOS `pgrep`/`ps` probes below; on Linux the
// probe reads `/proc` directly, so gating the import keeps the Linux build
// warning-clean.
#[cfg(target_os = "macos")]
use std::process::Command;

/// Cached variant of [`find_leaf_child`] + OS probe for the detector thread.
///
/// # Caching strategy (REF-033)
///
/// On macOS `find_leaf_child` calls `pgrep -P` up to 5 times per tick
/// (one fork+exec per tree hop), and `macos_check` calls `ps -o state=`.
/// At 500 ms tick intervals that is up to 6 process spawns per job per
/// second — a measurable CPU cost for large agent fleets.
///
/// The leaf PID of a stable agent TUI rarely changes after boot: claude
/// forks once, then stays at the same leaf for the session. We therefore
/// cache the last-computed leaf PID across ticks:
///
/// * If `cached_leaf` is `Some(leaf)` and `leaf` is still alive
///   (`kill(leaf, 0)` returns 0 — one syscall, no fork), reuse it and
///   skip the pgrep walk entirely.
/// * If the cached leaf is dead or absent, do the full `find_leaf_child`
///   walk and store the result for the next tick.
///
/// Detection behavior is preserved: the returned float is computed from
/// the same `linux_check`/`macos_check` call on the (possibly cached)
/// leaf PID. The liveness check is cheap (`kill(2)` with signal 0) and
/// never forks a child.
pub fn is_waiting_for_input_cached(pid: u32, cached_leaf: &mut Option<u32>) -> f32 {
    let leaf = resolve_leaf(pid, cached_leaf);
    probe_leaf(leaf)
}

fn probe_leaf(leaf: u32) -> f32 {
    #[cfg(target_os = "linux")]
    {
        linux_check(leaf)
    }

    #[cfg(target_os = "macos")]
    {
        macos_check(leaf)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = leaf;
        0.5
    }
}

/// Return the cached leaf PID if still alive, otherwise walk the tree
/// and update the cache. The liveness probe is `kill(pid, 0)` — one
/// syscall, never forks.
fn resolve_leaf(pid: u32, cached_leaf: &mut Option<u32>) -> u32 {
    if let Some(leaf) = *cached_leaf
        && is_pid_alive(leaf)
    {
        return leaf;
    }
    let leaf = find_leaf_child(pid);
    *cached_leaf = Some(leaf);
    leaf
}

/// Cheap single-PID liveness check via `kill(pid, 0)`.
/// Returns `true` if the process exists and we have permission to signal it.
fn is_pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: `kill(2)` with signal 0 does not send a signal — it
        // only checks whether the pid exists and we have permission.
        // The pid comes from our own process tree so it is always in
        // range for a `pid_t` cast.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true // conservative: assume alive on unsupported platforms
    }
}

/// Walk the process tree starting at `pid` to find the deepest
/// descendant. Returns `pid` itself when there are no children.
///
/// Bounded by a depth limit so a pathological forking process can
/// never spin us in a loop. Five hops covers every real-world wrapper
/// (claude has 1-2, codex has 1, vim has 0).
pub fn find_leaf_child(pid: u32) -> u32 {
    let mut current = pid;
    for _ in 0..5 {
        match first_child(current) {
            Some(child) if child != current => current = child,
            _ => break,
        }
    }
    current
}

#[cfg(target_os = "linux")]
fn first_child(pid: u32) -> Option<u32> {
    let path = format!("/proc/{pid}/task/{pid}/children");
    let contents = fs::read_to_string(&path).ok()?;
    contents
        .split_ascii_whitespace()
        .filter_map(|tok| tok.parse::<u32>().ok())
        .next()
}

#[cfg(target_os = "macos")]
fn first_child(pid: u32) -> Option<u32> {
    let out = Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .next()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn first_child(_pid: u32) -> Option<u32> {
    None
}

#[cfg(target_os = "linux")]
fn linux_check(pid: u32) -> f32 {
    // /proc/<pid>/wchan — the kernel symbol the task is sleeping on.
    // Names with `tty_read` / `n_tty_read` mean blocked on a TTY
    // read syscall, which is the canonical "waiting for input"
    // state.
    if let Ok(wchan) = fs::read_to_string(format!("/proc/{pid}/wchan")) {
        let w = wchan.trim();
        if w.contains("tty_read") || w.contains("n_tty_read") {
            return 1.0;
        }
        if w.contains("poll") || w.contains("select") || w.contains("epoll") {
            return 0.7;
        }
        if w == "0" || w.is_empty() {
            return 0.2;
        }
    }

    // /proc/<pid>/syscall — fall back: nr 0 (read) with fd 0 (stdin)
    // is "blocked on read of stdin". The fd argument follows the
    // syscall number in the file.
    if let Ok(s) = fs::read_to_string(format!("/proc/{pid}/syscall")) {
        let parts: Vec<&str> = s.split_whitespace().collect();
        if let Some(&nr) = parts.first() {
            if nr == "0" {
                if parts.get(1).map(|fd| *fd == "0x0").unwrap_or(false) {
                    return 1.0;
                }
                return 0.6;
            }
            // poll (7), pselect6 (270), select (23), epoll_pwait (281).
            if matches!(nr, "7" | "23" | "270" | "281") {
                return 0.7;
            }
        }
    }

    0.5
}

#[cfg(target_os = "macos")]
fn macos_check(pid: u32) -> f32 {
    // macOS has no /proc. `ps -o state=` returns a one-letter code:
    //   R = runnable, S = sleeping (interruptible), I = idle,
    //   U = uninterruptible wait, T = stopped, Z = zombie.
    // "Sleeping" usually means waiting on I/O (incl. tty_read /
    // poll), which is what we want.
    if let Ok(out) = Command::new("ps")
        .args(["-o", "state=", "-p", &pid.to_string()])
        .output()
    {
        let state = String::from_utf8_lossy(&out.stdout);
        let state = state.trim();
        if state.starts_with('S') {
            return 0.85;
        }
        if state.starts_with('I') {
            // Idle thread (commonly seen on macOS for processes
            // blocked on `select`). Treat as "waiting" but with
            // slightly lower confidence than 'S'.
            return 0.8;
        }
        if state.starts_with('R') {
            return 0.2;
        }
    }
    0.5
}

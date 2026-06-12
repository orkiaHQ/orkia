// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Auto-foreground PTY relay for a detached single-agent runtime.
//!
//! detached *runtime* process whose controlling terminal (stdin/stdout) is a
//! PTY the daemon captures (call it `ttysA`). That runtime then spawns the real
//! agent (claude) in-process on its OWN nested PTY (`ttysB`). The daemon's
//! `TerminalEngine` only sees `ttysA` — the runtime's wrapper status lines, NOT
//! the claude TUI on `ttysB`. So `attach` reaches the wrapper, not the agent.
//!
//! This relay closes that gap. Inside the detached runtime, right after the
//! single agent spawns, it splices the agent's PTY (`ttysB`) ↔ the runtime's
//! controlling terminal (`ttysA`):
//!
//! ```text
//!   ttysA stdin (libc::read) ─► agent PTY writer ─► claude
//!         ▲                                            │
//!   user keystrokes                              claude writes
//!   (relayed by daemon)                               │
//!                                                      ▼
//!   ttysA stdout ◄── output thread ◄── subscribe_output() ◄── engine reader
//! ```
//!
//! The full bridge is then:
//! `user terminal ↔ [daemon attach] ↔ ttysA ↔ [this relay] ↔ ttysB (claude)`.
//! With the relay running, the daemon's engine mirrors claude directly, so
//! `attach` shows the live TUI and `tell`-injected keystrokes land in the
//! persistent session.
//!
//! Design constraints (CLAUDE.md):
//! - **#1 REPL loop sacred.** Both directions run on dedicated OS threads; the
//!   runtime's `run_one_command` poll loop keeps draining events.
//! - **Output is passive.** A second `read(2)` on the agent's PTY master would
//!   race the engine's own reader (PTY master read is not multi-consumer), so we
//!   mirror via [`TerminalEngine::subscribe_output`] — a fan-out the reader
//!   feeds — never a second fd reader.
//!
//! The forward (`stdin → PTY`) loop mirrors `raw_attach::run_forward_input_loop`
//! but is purpose-built: a relay forwards EVERY byte verbatim (no Ctrl-Z/Ctrl-\
//! detach interception — the runtime has no "detach", it IS the session) and
//! watches the engine's child-exited flag instead of an attach-detach channel.

use std::io::{self, Write};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use orkia_pty::SharedWriter;
use orkia_terminal_core::TerminalEngine;

use super::raw_termios::RawModeGuard;

/// RAII handle for an active foreground relay. Holding it keeps both I/O
/// threads alive and `ttysA` in raw mode; dropping it stops the threads,
/// joins them, and restores the terminal. In the normal detached-runtime
/// path the process exits via `process::exit` (skipping `Drop`), and the OS
/// reaps the threads — the explicit teardown matters only on the error /
/// early-return paths that unwind through `run_one_command`.
pub(crate) struct ForegroundRelay {
    stop: Arc<AtomicBool>,
    input: Option<JoinHandle<()>>,
    output: Option<JoinHandle<()>>,
    // Restores `ttysA` termios on drop. Field order matters: declared last so
    // it drops after the join handles above (threads stop before raw mode is
    // lifted, avoiding a window where the slave is cooked mid-relay).
    _raw: RawModeGuard,
}

impl ForegroundRelay {
    /// Splice `engine`'s PTY ↔ this process's controlling terminal. Subscribes
    /// to the live output stream BEFORE painting the catch-up snapshot so no
    /// chunk emitted after the snapshot is lost. Returns an error only when
    /// raw mode or a relay thread fails to start.
    pub(crate) fn start(engine: &TerminalEngine) -> io::Result<Self> {
        let raw = RawModeGuard::enter()?;
        let stop = Arc::new(AtomicBool::new(false));
        let child_exited = engine.child_exited_handle();

        // Subscribe first (no gap after the snapshot), then paint the current
        // visible grid so `ttysA` catches up on whatever the agent drew before
        // the relay attached. At spawn time this is near-empty; the snapshot is
        // belt-and-suspenders for the case where the relay starts slightly late.
        let rx = engine.subscribe_output();
        paint_snapshot(engine);

        let output = spawn_output(rx, Arc::clone(&stop), child_exited)?;
        let input = match spawn_input(engine.writer(), Arc::clone(&stop)) {
            Ok(handle) => handle,
            Err(e) => {
                // Output thread already running — stop and join it before
                // surfacing the failure so we never leak a relay thread.
                stop.store(true, Ordering::SeqCst);
                let _ = output.join();
                return Err(e);
            }
        };

        Ok(Self {
            stop,
            input: Some(input),
            output: Some(output),
            _raw: raw,
        })
    }
}

impl Drop for ForegroundRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.input.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.output.take() {
            let _ = handle.join();
        }
    }
}

/// Paint the agent engine's current visible grid to `ttysA` stdout so the
/// daemon's capture reflects the agent's screen immediately on relay start.
fn paint_snapshot(engine: &TerminalEngine) {
    let snapshot = engine.render_visible_snapshot();
    if snapshot.is_empty() {
        return;
    }
    let mut out = io::stdout().lock();
    let _ = out.write_all(&snapshot);
    let _ = out.flush();
}

/// Agent PTY output → `ttysA` stdout. Drains the passive subscriber channel
/// the engine reader fans out to and writes each chunk verbatim. The
/// subscriber channel does NOT disconnect when the agent exits, so we poll the
/// engine's `child_exited` flag on each idle tick and stop once it is set.
fn spawn_output(
    rx: mpsc::Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
    child_exited: Arc<AtomicBool>,
) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("orkia-fg-relay-out".into())
        .spawn(move || {
            let stdout = io::stdout();
            loop {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(bytes) => {
                        let mut handle = stdout.lock();
                        if handle
                            .write_all(&bytes)
                            .and_then(|()| handle.flush())
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if child_exited.load(Ordering::SeqCst) {
                            return;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        })
}

/// `ttysA` stdin → agent PTY writer, on a dedicated thread.
fn spawn_input(writer: SharedWriter, stop: Arc<AtomicBool>) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("orkia-fg-relay-in".into())
        .spawn(move || forward_stdin_to_pty(writer, stop))
}

/// Forward-only `stdin → PTY` pump. Raw `poll(2)` + `read(2)`, no parsing, no
/// detach interception: every byte goes to the agent. Exits when `stop` is set
/// (relay torn down) or stdin closes (`ttysA` gone — the daemon dropped it).
fn forward_stdin_to_pty(writer: SharedWriter, stop: Arc<AtomicBool>) {
    const STDIN: RawFd = libc::STDIN_FILENO;
    const POLL_TIMEOUT_MS: i32 = 100;
    let mut buf = [0u8; 4096];
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let mut pfd = libc::pollfd {
            fd: STDIN,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: pfd is a valid pollfd; count=1; timeout is small non-negative.
        let ret = unsafe { libc::poll(&mut pfd, 1, POLL_TIMEOUT_MS) };
        if ret < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return;
        }
        if ret == 0 {
            continue;
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return;
        }
        if pfd.revents & libc::POLLIN == 0 {
            continue;
        }
        // SAFETY: buf is valid for buf.len() bytes; STDIN is a valid fd.
        let n = unsafe { libc::read(STDIN, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return;
        }
        if n == 0 {
            return;
        }
        let mut w = writer.lock();
        if w.write_all(&buf[..n as usize])
            .and_then(|()| w.flush())
            .is_err()
        {
            return;
        }
    }
}

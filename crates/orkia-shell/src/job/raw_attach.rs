// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Raw byte splice pump for attach mode.
//!
//! Architecture (vs. the previous crossterm-based pump that this
//! replaces):
//!
//! ```text
//!   stdin (libc::read)  ──►  PTY master writer  ──►  child PTY slave
//!         ▲                                                 │
//!         │                                                 ▼
//!   raw bytes                                          child writes
//!   (no parsing)                                            │
//!                                                           ▼
//!   stdout  ◄──  output thread  ◄──  raw_output_rx  ◄──  engine reader
//! ```
//!
//! Why this works where the previous architecture didn't:
//!
//! - **No crossterm in the splice loop.** crossterm's `enable_raw_mode`
//!   and event reader negotiate terminal features (bracketed paste,
//!   focus events, kitty keyboard) by writing query escapes to the
//!   tty. iTerm replies with DCS capability strings
//!   (`\x1bP>|iTerm2 3.6.10\x1b\\`). The previous code re-parsed those
//!   replies as 18 individual KeyEvents and forwarded them to the
//!   child as if the user had typed them, corrupting the agent's input
//!   state before any real keystroke. Going through raw `read(2)` on a
//!   fresh termios `cfmakeraw`'d STDIN avoids ALL of that.
//! - **No re-encoding of escape sequences.** Multi-byte sequences like
//!   `\x1b[B` (arrow Down) pass through verbatim instead of being
//!   parsed into a `KeyEvent::Down` and re-encoded back to `\x1b[B`.
//!   Same for DCS, mouse reports, OSC queries — whatever the child
//!   wrote out, the child sees the same bytes back when the user
//!   replies. No translation layer to drop or fabricate bytes.
//! - **Ctrl-C as a byte goes to the child.** Because we `cfmakeraw`'d
//!   STDIN (`ISIG` off), the kernel doesn't generate `SIGINT` from
//!   `0x03` — instead our `read(2)` returns the byte and we write it
//!   to the child's PTY master. The child's PTY slave still has its
//!   own termios; whatever the child set up there decides how to
//!   handle the byte. This is exactly the behaviour the user expects
//!   from a real terminal.
//! - **Ctrl-Z and Ctrl-\ detach.** The bytes `0x1a` / `0x1c` are
//!   intercepted before forwarding; any bytes that preceded one in the
//!   same read buffer are flushed to the child first. Without the
//!   Ctrl-\ intercept the byte would reach the child PTY (whose line
//!   discipline still has `ISIG` on) and the kernel would deliver
//!   SIGQUIT, killing the agent.
//!
//! `poll(2)` on both stdin and the master fd) is the output direction:
//! the agent job's `TerminalEngine` already owns a reader thread that
//! `read(2)`s the master fd and feeds a `raw_output_rx` channel
//! (plus the alacritty grid for re-attach state and the OSC-133 block
//! parser). Adding a second reader on the master fd would race the
//! engine's reader for bytes — kernel `read(2)` on a PTY master is
//! not multi-consumer. So we route output through the engine's
//! existing channel, which is still byte-perfect — the engine reader
//! pushes `Vec<u8>` chunks verbatim onto the channel without any
//! parsing.

use std::io::{self, Write};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use orkia_pty::SharedWriter;
use orkia_terminal_core::RawOutputRx;

use super::raw_termios::RawModeGuard;
use crate::error::ShellError;
use crate::job::entry::JobEntry;

/// Exit code returned by [`run_foreground`] when the user detached.
/// Kept as the historical `-100` sentinel because the REPL's
/// `is_detach()` discriminator depends on it.
const DETACH_CODE: i32 = -100;

/// Bytes that trigger detach. Both are advertised in the attach
/// banner. `0x1a` is Ctrl-Z (SUSP) — what tmux and most TUIs treat as
/// "leave this sub-session". `0x1c` is Ctrl-\ (QUIT) — without this
/// intercept the byte would reach the child PTY, whose line discipline
/// still has `ISIG` on, and the kernel would deliver SIGQUIT to the
/// agent (claude dumps and exits with status 1).
const DETACH_BYTES: &[u8] = &[
    0x1a, // Ctrl-Z (SUSP)
    0x1c, // Ctrl-\ (QUIT)
    0x1d, // Ctrl-] (telnet/ssh-style escape — usually free of iTerm remaps)
    0x1e, // Ctrl-^
];

/// xterm `modifyOtherKeys` CSI-u encodings sent by terminals (notably
/// iTerm with the mode that claude / other TUI agents turn on) instead
/// of the raw Ctrl-byte. The first decimal is ASCII of the unmodified
/// key; `5` is the Ctrl modifier. Without this list, Ctrl-Z while
/// claude is in the foreground arrives as `ESC [ 27;5;122 ~` and our
/// single-byte detach scan misses it entirely.
const DETACH_CSI_SEQS: &[&[u8]] = &[
    b"\x1b[27;5;122~", // Ctrl-Z (lowercase z)
    b"\x1b[27;5;90~",  // Ctrl-Shift-Z (uppercase Z) — same physical key
    b"\x1b[27;5;28~",  // Ctrl-\
    b"\x1b[27;5;29~",  // Ctrl-]
    b"\x1b[27;5;30~",  // Ctrl-^
];

/// Find the earliest detach hit in `buf` — either a one-byte Ctrl-key
/// from [`DETACH_BYTES`] or a multi-byte modifyOtherKeys CSI from
/// [`DETACH_CSI_SEQS`]. Returns `(start, len)` so the caller can flush
/// the prefix before detaching.
fn detach_match(buf: &[u8]) -> Option<(usize, usize)> {
    let single = buf
        .iter()
        .position(|b| DETACH_BYTES.contains(b))
        .map(|p| (p, 1));
    let csi = DETACH_CSI_SEQS
        .iter()
        .filter_map(|needle| find_subslice(buf, needle).map(|p| (p, needle.len())))
        .min_by_key(|(p, _)| *p);
    match (single, csi) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// PID of the currently-attached child, or `0` if no attach is active.
/// Read by the binary's SIGINT swallow handler so a Ctrl-C delivered
/// as a signal (e.g. iTerm "Send Signal: INT" binding) can still be
/// forwarded to the child the user is looking at.
static ATTACHED_PID: AtomicI32 = AtomicI32::new(0);

/// Set by the binary's SIGQUIT handler to ask the active attach loop
/// to detach at its next poll. Cleared on attach entry.
static DETACH_REQUESTED: AtomicBool = AtomicBool::new(false);

/// PID of the currently-attached child, or `None` if no attach is active.
pub fn attached_pid() -> Option<i32> {
    let p = ATTACHED_PID.load(Ordering::SeqCst);
    if p > 0 { Some(p) } else { None }
}

/// Request the active attach loop to detach (signal-driven path).
pub fn request_detach() {
    DETACH_REQUESTED.store(true, Ordering::SeqCst);
}

/// Test whether a [`run_foreground`] exit code indicates a user detach.
pub const fn is_detach(code: i32) -> bool {
    code == DETACH_CODE
}

/// Outcome of the splice pump. Internal — translated to the legacy
/// `i32` exit code shape by [`run_foreground`] for REPL compatibility.
enum AttachResult {
    /// User pressed Ctrl-Z (the detach byte) or a SIGQUIT was routed
    /// to the detach request flag.
    Detached,
    /// PTY master closed / engine reader thread exited — the child is
    /// gone. Exit code (if known) is forwarded.
    ChildExited(Option<i32>),
    /// Unrecoverable I/O error during the splice.
    Error(String),
}

/// Foreground-attach entry point used by the REPL.
///
/// Returns [`DETACH_CODE`] (`-100`) on detach, the child's exit code on
/// child exit, or `-1` on internal error. Matches the contract of the
/// crossterm-based implementation it replaces so `repl.rs` does not
/// need to change.
pub async fn run_foreground(entry: &mut JobEntry) -> Result<i32, ShellError> {
    tracing::debug!(
        job_id = %entry.id,
        pid = ?entry.pid(),
        "raw_attach: entering raw mode",
    );

    let raw_rx = entry
        .engine
        .take_raw_output_rx()
        .ok_or_else(|| ShellError::Other("raw output receiver already taken".into()))?;
    let writer = entry.engine.writer();

    // Termios cfmakeraw on STDIN. cfmakeraw clears ISIG so Ctrl-C is
    // delivered as the byte 0x03 rather than as a SIGINT to orkia.
    let _termios_guard =
        RawModeGuard::enter().map_err(|e| ShellError::Other(format!("raw mode: {e}")))?;

    // Register the live child PID + reset stale detach flag.
    let child_pid: i32 = entry.pid().map(|p| p as i32).unwrap_or(0);
    if child_pid > 0 {
        ATTACHED_PID.store(child_pid, Ordering::SeqCst);
    }
    DETACH_REQUESTED.store(false, Ordering::SeqCst);
    let _pid_guard = AttachedPidGuard;

    replay_visible_snapshot(&raw_rx, entry);

    // Run the splice on a blocking thread — both `poll(2)` and
    // `read(2)` block, and we don't want them tying up a tokio worker.
    // We still keep an async wrapper so the REPL's control dispatcher
    // (`dispatch_named`) can `.await` the result and so we can periodically
    // check the child's exit code from the tokio side.
    let result = run_splice(writer, raw_rx, entry).await;

    // Detach cleanup must run before guards drop (raw mode off), so do it first.
    let outcome = detach_cleanup_and_translate(result);
    drop(_pid_guard);
    drop(_termios_guard);
    outcome
}

/// Drain the buffered raw-output backlog, then render the alacritty grid
/// snapshot to stdout. This ensures the user sees exactly one copy of the
/// child's current screen on attach, not N stacked copies from replaying
/// the raw history ring.
fn replay_visible_snapshot(raw_rx: &RawOutputRx, entry: &mut JobEntry) {
    // Drain the backlog FIRST: those bytes are already reflected in the
    // live grid (agent engines keep it live via `persistent_program`).
    // If we let them replay after the snapshot, the user sees the UI twice.
    while raw_rx.try_recv().is_ok() {}
    let snapshot = entry.engine.render_visible_snapshot();
    if !snapshot.is_empty() {
        let mut out = io::stdout().lock();
        let _ = out.write_all(&snapshot);
        let _ = out.flush();
    }
}

/// Emit detach cleanup sequences and translate an [`AttachResult`] to the
/// legacy `i32` exit code that the REPL expects.
fn detach_cleanup_and_translate(result: AttachResult) -> Result<i32, ShellError> {
    // Detach cleanup: emit terminal-state-reset sequences to stdout
    // so the shell prompt comes back on a clean line. Without this,
    // any of claude's lingering cursor positioning / alt-screen /
    // mouse-reporting / bracketed-paste / focus-reporting state stays
    // active and the next prompt overlaps the TUI artifacts.
    if matches!(result, AttachResult::Detached) {
        emit_detach_cleanup();
    }
    match result {
        AttachResult::Detached => Ok(DETACH_CODE),
        AttachResult::ChildExited(code) => Ok(code.unwrap_or(0)),
        AttachResult::Error(msg) => {
            tracing::error!("raw_attach: {msg}");
            Ok(-1)
        }
    }
}

/// Drive the splice. Spawns an output thread for `PTY -> stdout`,
/// runs the input loop on a blocking task for `stdin -> PTY`.
async fn run_splice(
    writer: SharedWriter,
    raw_rx: RawOutputRx,
    entry: &mut JobEntry,
) -> AttachResult {
    let stop = Arc::new(AtomicBool::new(false));
    let child_exit_signal = Arc::new(AtomicBool::new(false));

    let output_thread = spawn_output_thread(raw_rx, stop.clone(), child_exit_signal.clone());

    // The input loop runs in spawn_blocking so the blocking `poll(2)`
    // and `read(2)` calls don't pin a tokio worker.
    let input_stop = stop.clone();
    let mut input_handle = tokio::task::spawn_blocking(move || run_input_loop(writer, input_stop));

    let outcome = await_input_or_exit(&mut input_handle, &child_exit_signal, entry).await;

    // Tell both threads to stop. The input loop checks `stop` between
    // poll cycles; the output thread checks it between recv_timeouts.
    stop.store(true, Ordering::SeqCst);

    // If the input task is still running (detach / child-exit paths),
    // give it up to one poll cycle (~100 ms) to see the stop flag and
    // exit gracefully, then abort it.
    if !input_handle.is_finished() {
        input_handle.abort();
    }
    if let Ok(OutputThreadResult::Detached(rx)) = output_thread.join() {
        // Put the raw byte channel back so a follow-up `attach` on
        // the same job can take it again. Without this, re-attach
        // fails with "raw output receiver already taken".
        entry.engine.restore_raw_output_rx(rx);
    }

    outcome
}

/// Poll the 50 ms tick loop until the input task finishes, the child exits,
/// or a detach signal arrives. On each iteration also propagates terminal
/// resize events to the attached PTY.
async fn await_input_or_exit(
    input_handle: &mut tokio::task::JoinHandle<AttachResult>,
    child_exit_signal: &Arc<AtomicBool>,
    entry: &mut JobEntry,
) -> AttachResult {
    let master = entry.engine.master();
    let screen = entry.engine.screen();
    let dims = entry.engine.dims();
    loop {
        if let Some(code) = entry.try_exit_code() {
            return AttachResult::ChildExited(Some(code));
        }
        if child_exit_signal.load(Ordering::SeqCst) {
            return AttachResult::ChildExited(entry.try_exit_code());
        }
        if DETACH_REQUESTED.swap(false, Ordering::SeqCst) {
            tracing::debug!("raw_attach: detach requested via signal");
            return AttachResult::Detached;
        }
        if input_handle.is_finished() {
            return match input_handle.await {
                Ok(r) => r,
                Err(e) => AttachResult::Error(format!("input task join: {e}")),
            };
        }
        poll_winsize_and_resize(&master, &screen, &dims);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// SIGWINCH-equivalent: read the host terminal size and propagate to the
/// attached PTY when it changes. Called each 50 ms tick. Cheaper than
/// installing a `libc::signal` handler; the user's resize is human-paced
/// (≥ 100 ms granularity perceptible).
fn poll_winsize_and_resize(
    master: &Option<orkia_pty::SharedMaster>,
    screen: &orkia_pty::ScreenTerm,
    dims: &orkia_pty::SharedDims,
) {
    if let Some((cols, rows)) = read_terminal_size_via_tiocgwinsz()
        && let Some(master) = master.as_ref()
    {
        let (current_cols, current_rows) = *dims.lock();
        if (cols, rows) != (current_cols, current_rows) {
            orkia_terminal_core::apply_resize(master, screen, dims, cols, rows);
        }
    }
}

/// Promote a *running* foreground shell command (in brush's PTY) to a live
/// raw splice, for the rest of its lifetime.
///
/// Used by `BrushSession::execute` the moment it detects the command went
/// full-screen (alt-screen enter). Reuses the same proven primitives as the
/// agent attach (`run_input_loop` stdin→PTY, `spawn_output_thread`
/// PTY→stdout, `RawModeGuard`, poll-driven SIGWINCH) but the exit condition is
/// "the command's `exec` future completed" rather than a JobEntry exit code —
/// so it takes the in-flight future and drives it to completion inside the
/// splice loop.
///
/// `initial` is the output already captured before promotion (the program's
/// first alt-screen draw); it is flushed to stdout before live streaming so
/// nothing is lost. Drives `exec` to completion and hands `raw_rx` back so the
/// session keeps reading its PTY for the next command.
///
/// No Ctrl-Z detach here: a brush foreground command isn't a backgroundable
/// `JobController` job, so every byte (incl. Ctrl-C/Ctrl-Z) is forwarded to
/// the child, which owns its own PTY line discipline. The only exit is the
/// command finishing.
pub(crate) async fn splice_brush_foreground<F, T>(
    writer: SharedWriter,
    raw_rx: RawOutputRx,
    pty: &orkia_terminal_core::TerminalEngine,
    initial: &[u8],
    mut exec: std::pin::Pin<&mut F>,
) -> (T, RawOutputRx)
where
    F: std::future::Future<Output = T>,
{
    // Raw mode on the host stdin (ISIG off → Ctrl-C reaches the child as a
    // byte, not a SIGINT to orkia). Restored on drop.
    let termios = match RawModeGuard::enter() {
        Ok(g) => Some(g),
        Err(e) => {
            tracing::warn!(error = %e, "promote: raw mode failed; finishing captured");
            return (exec.as_mut().await, raw_rx);
        }
    };
    // Flush the pre-promotion draw (alt-screen enter + first frame).
    if !initial.is_empty() {
        let mut out = io::stdout().lock();
        let _ = out.write_all(initial);
        let _ = out.flush();
    }

    let stop = Arc::new(AtomicBool::new(false));
    let child_exit_signal = Arc::new(AtomicBool::new(false));
    let output_thread = spawn_output_thread(raw_rx, stop.clone(), child_exit_signal.clone());
    let input_handle = {
        let w = writer;
        let s = stop.clone();
        tokio::task::spawn_blocking(move || run_forward_input_loop(w, s))
    };

    let master = pty.master();
    let screen = pty.screen();
    let dims = pty.dims();

    // Drive the command to completion; while it runs, the I/O threads splice
    // stdin↔PTY and PTY→stdout, and we propagate host resizes (SIGWINCH).
    let exec_out = loop {
        tokio::select! {
            biased;
            r = exec.as_mut() => break r,
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                if let Some((cols, rows)) = read_terminal_size_via_tiocgwinsz()
                    && let Some(master) = master.as_ref()
                {
                    let (cc, cr) = *dims.lock();
                    if (cols, rows) != (cc, cr) {
                        orkia_terminal_core::apply_resize(master, &screen, &dims, cols, rows);
                    }
                }
            }
        }
    };

    // Settle so the program's teardown (rmcup / cursor restore) reaches the
    // host before we stop the output thread; then tear down. RawModeGuard
    // restores the host termios on drop.
    tokio::time::sleep(Duration::from_millis(60)).await;
    stop.store(true, Ordering::SeqCst);
    input_handle.abort();
    let returned = match output_thread.join() {
        Ok(OutputThreadResult::Detached(rx)) => Some(rx),
        _ => None,
    };
    drop(termios);

    // Hand the channel back. On the rare "PTY disconnected" path the output
    // thread couldn't return it; a fresh dummy keeps the session alive (the
    // next command re-binds via brush's PTY anyway).
    let raw_rx = returned.unwrap_or_else(|| {
        let (_tx, rx) = mpsc::channel();
        rx
    });
    (exec_out, raw_rx)
}

/// Forward-only stdin → PTY pump for a promoted foreground shell command.
/// Unlike [`run_input_loop`] it does NOT intercept Ctrl-Z/Ctrl-\: every byte
/// is forwarded to the child (which owns its own PTY line discipline). Exits
/// when `stop` is set (the command finished) or stdin closes.
fn run_forward_input_loop(writer: SharedWriter, stop: Arc<AtomicBool>) -> AttachResult {
    const STDIN: RawFd = libc::STDIN_FILENO;
    const POLL_TIMEOUT_MS: i32 = 100;
    let mut buf = [0u8; 4096];
    loop {
        if stop.load(Ordering::SeqCst) {
            return AttachResult::Detached;
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
            return AttachResult::Error(format!("poll(stdin): {e}"));
        }
        if ret == 0 {
            continue;
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return AttachResult::Detached;
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
            return AttachResult::Error(format!("read(stdin): {e}"));
        }
        if n == 0 {
            return AttachResult::Detached;
        }
        let mut w = writer.lock();
        if w.write_all(&buf[..n as usize]).is_err() {
            return AttachResult::Error("write(pty)".into());
        }
        let _ = w.flush();
    }
}

/// stdin -> PTY master. Raw `poll(2)` + `read(2)`. No parsing. The
/// detach byte (`0x1a`) is intercepted before forwarding; any bytes
/// preceding it in the same read are sent first so a paste like
/// `"hello\x1a"` flushes `hello` and then detaches.
fn run_input_loop(writer: SharedWriter, stop: Arc<AtomicBool>) -> AttachResult {
    const STDIN: RawFd = libc::STDIN_FILENO;
    const POLL_TIMEOUT_MS: i32 = 100;
    let mut buf = [0u8; 4096];

    loop {
        if stop.load(Ordering::SeqCst) {
            return AttachResult::Detached;
        }

        let mut pfd = libc::pollfd {
            fd: STDIN,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: pfd is a valid pollfd; count=1 matches the slice
        // size; timeout is a small non-negative int.
        let ret = unsafe { libc::poll(&mut pfd, 1, POLL_TIMEOUT_MS) };

        if ret < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return AttachResult::Error(format!("poll(stdin): {e}"));
        }
        if ret == 0 {
            // Timeout — re-check stop flag and loop.
            continue;
        }

        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            // stdin closed — treat as detach.
            return AttachResult::Detached;
        }

        if pfd.revents & libc::POLLIN == 0 {
            continue;
        }

        // SAFETY: buf is valid for `buf.len()` bytes; STDIN is a valid fd.
        let n = unsafe { libc::read(STDIN, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return AttachResult::Error(format!("read(stdin): {e}"));
        }
        if n == 0 {
            return AttachResult::Detached;
        }

        let bytes = &buf[..n as usize];
        let hex = bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        tracing::debug!(
            bytes_len = bytes.len(),
            hex = %hex,
            "raw_attach: stdin bytes",
        );

        // Detach byte handling: forward anything before it, then exit.
        if let Some((pos, _len)) = detach_match(bytes) {
            if pos > 0 {
                let mut w = writer.lock();
                let _ = w.write_all(&bytes[..pos]);
                let _ = w.flush();
            }
            return AttachResult::Detached;
        }

        let mut w = writer.lock();
        if let Err(e) = w.write_all(bytes) {
            return AttachResult::Error(format!("write(pty): {e}"));
        }
        let _ = w.flush();
    }
}

/// PTY master output -> stdout. Drains the engine's raw byte channel
/// and writes each chunk verbatim. The channel close (engine reader
/// thread exiting because the master returned EOF) is how we learn
/// the child exited.
fn spawn_output_thread(
    raw_rx: RawOutputRx,
    stop: Arc<AtomicBool>,
    child_exit_signal: Arc<AtomicBool>,
) -> std::thread::JoinHandle<OutputThreadResult> {
    std::thread::spawn(move || {
        let stdout = io::stdout();
        loop {
            if stop.load(Ordering::SeqCst) {
                // Detach path: hand the receiver back so the engine
                // can lend it to a future re-attach.
                return OutputThreadResult::Detached(raw_rx);
            }
            match raw_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(bytes) => {
                    let mut handle = stdout.lock();
                    if handle.write_all(&bytes).is_err() {
                        return OutputThreadResult::Detached(raw_rx);
                    }
                    let _ = handle.flush();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // Engine reader thread closed the channel — the
                    // child PTY hit EOF. No receiver to return.
                    child_exit_signal.store(true, Ordering::SeqCst);
                    return OutputThreadResult::Disconnected;
                }
            }
        }
    })
}

/// Outcome of the PTY → stdout drainer. On detach we hand the receiver
/// back so the engine can be re-attached; on child exit the channel is
/// already gone.
enum OutputThreadResult {
    Detached(RawOutputRx),
    Disconnected,
}

/// Reset terminal state on detach. The child may have left the user
/// TTY in alt-screen, mouse-reporting, bracketed-paste, or focus-event
/// mode, and the cursor may be mid-frame with a non-default SGR. Emit
/// the standard `rmcup` / cursor-show / SGR-reset / mouse-off /
/// bracketed-paste-off / focus-off sequences so the next shell prompt
/// renders on a fresh line.
fn emit_detach_cleanup() {
    use std::io::Write;
    let seq = concat!(
        "\x1b[?1049l",                                  // leave alt-screen (no-op if not in)
        "\x1b[?25h",                                    // cursor visible
        "\x1b[0m",                                      // reset SGR
        "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l", // mouse off
        "\x1b[?2004l",                                  // bracketed paste off
        "\x1b[?1004l",                                  // focus reporting off
        "\x1b[H",                                       // cursor to top-left
        "\x1b[2J",                                      // clear entire screen — drops claude's TUI
        // artifacts that were drawn in main screen at
        // fixed rows; without this they remain visible
        // behind the next shell prompt.
        "\x1b[3J", // clear scrollback so re-attach starts fresh
    );
    let mut out = io::stdout().lock();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// RAII clear for `ATTACHED_PID` so a panic / early return never
/// leaves a stale PID pointing at a long-dead child.
struct AttachedPidGuard;

impl Drop for AttachedPidGuard {
    fn drop(&mut self) {
        ATTACHED_PID.store(0, Ordering::SeqCst);
    }
}

/// Read the host terminal's current `(cols, rows)` via `TIOCGWINSZ`
/// on stdin. Returns `None` if stdin is not a tty (test contexts,
/// pipes, redirects). Polled each tick of the attach main loop to
/// propagate user-initiated resizes to the attached child's PTY —
/// the bash/tmux equivalent of SIGWINCH forwarding, but driven by
/// poll instead of a signal handler (avoids global state + a
/// signal_hook dependency).
pub(crate) fn read_terminal_size_via_tiocgwinsz() -> Option<(usize, usize)> {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ is the standard read-window-size ioctl;
    // libc::STDIN_FILENO is a valid fd (0). `winsize` is the
    // documented argument type.
    let rc = unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut ws) };
    if rc != 0 || ws.ws_col == 0 || ws.ws_row == 0 {
        return None;
    }
    Some((ws.ws_col as usize, ws.ws_row as usize))
}

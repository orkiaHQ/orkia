// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;

use orkia_shell::ShellConfig;

use super::protocol::{Request, Response, send_request, socket_path};

/// One-byte detach triggers — mirrors raw_attach.rs `DETACH_BYTES`.
/// `0x1a` = Ctrl-Z, `0x1c` = Ctrl-\, `0x1d` = Ctrl-], `0x1e` = Ctrl-^.
const DETACH_BYTES: &[u8] = &[0x1a, 0x1c, 0x1d, 0x1e];

/// CSI-u (modifyOtherKeys) multi-byte encodings sent by iTerm and other
/// modern terminals when an agent has enabled that mode. Without these,
/// Ctrl-Z from iTerm arrives as `\x1b[27;5;122~` and the single-byte
/// scan above misses it entirely. Sequences are verbatim from
/// raw_attach.rs `DETACH_CSI_SEQS`.
const DETACH_CSI_SEQS: &[&[u8]] = &[
    b"\x1b[27;5;122~", // Ctrl-Z (lowercase z)
    b"\x1b[27;5;90~",  // Ctrl-Shift-Z (uppercase Z) — same physical key
    b"\x1b[27;5;28~",  // Ctrl-\
    b"\x1b[27;5;29~",  // Ctrl-]
    b"\x1b[27;5;30~",  // Ctrl-^
];

/// Terminal-state reset emitted after detach or job exit, matching
/// `emit_detach_cleanup()` in raw_attach.rs verbatim.
const DETACH_CLEANUP: &[u8] = concat!(
    "\x1b[?1049l",                                  // leave alt-screen
    "\x1b[?25h",                                    // cursor visible
    "\x1b[0m",                                      // reset SGR
    "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l", // mouse off
    "\x1b[?2004l",                                  // bracketed paste off
    "\x1b[?1004l",                                  // focus reporting off
    "\x1b[H",                                       // cursor to top-left
    "\x1b[2J",                                      // clear screen
    "\x1b[3J",                                      // clear scrollback
)
.as_bytes();

/// Find the earliest detach hit in `buf` — either a one-byte Ctrl-key
/// from [`DETACH_BYTES`] or a multi-byte CSI-u sequence from
/// [`DETACH_CSI_SEQS`]. Returns `(start, len)` so the caller can flush
/// the prefix before detaching. Mirrors raw_attach.rs `detach_match`.
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

/// Read the attaching terminal's `(cols, rows)` via `TIOCGWINSZ` on
/// stdin. Returns `None` when stdin is not a tty (tests, pipes,
/// redirects). Mirrors raw_attach.rs `read_terminal_size_via_tiocgwinsz`.
fn terminal_size() -> Option<(u16, u16)> {
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
    Some((ws.ws_col, ws.ws_row))
}

pub(super) fn client(config: &ShellConfig, id: u32, target: Option<String>) -> Result<(), String> {
    let path = socket_path(&config.data_dir);
    let mut stream =
        UnixStream::connect(&path).map_err(|e| format!("connect {}: {e}", path.display()))?;
    // Carry the attaching terminal's size so the runtime resizes the agent
    // PTY before the catch-up paint — a TUI agent (claude, vim inside it)
    // renders garbage at the spawn-time 120×42 fallback otherwise. `None`
    // when stdin is not a tty (tests, pipes): the agent keeps its size.
    let (cols, rows) = match terminal_size() {
        Some((c, r)) => (Some(c), Some(r)),
        None => (None, None),
    };
    send_request(
        &mut stream,
        &Request::Attach {
            id,
            target,
            cols,
            rows,
        },
    )?;
    let reader_stream = stream
        .try_clone()
        .map_err(|e| format!("clone attach stream: {e}"))?;
    let mut reader = BufReader::new(reader_stream);
    let mut header = String::new();
    reader
        .read_line(&mut header)
        .map_err(|e| format!("read attach header: {e}"))?;
    let resp: Response =
        serde_json::from_str(&header).map_err(|e| format!("parse attach header: {e}"))?;
    if !resp.ok {
        return Err(resp
            .error
            .unwrap_or_else(|| "daemon attach failed".to_string()));
    }
    eprintln!("  \x1b[90m[daemon:{id}] attached - Ctrl-Z to detach\x1b[0m");
    // Raw mode for the splice: without it a standalone `orkia attach` runs
    // in the shell's canonical mode — keystrokes are line-buffered and ISIG
    // turns the Ctrl-Z detach byte into a SIGTSTP that suspends THIS client.
    // Nested entry under the REPL's own `RawModeGuard` is a no-op (shared
    // snapshot in `raw_termios`), and a non-tty stdin (pipes, tests) simply
    // fails to enter — the splice still runs, there is just no tty to fix.
    let _raw = orkia_shell::job::raw_termios::RawModeGuard::enter();
    run_client_io(stream)
}

fn run_client_io(mut stream: UnixStream) -> Result<(), String> {
    let mut output = stream
        .try_clone()
        .map_err(|e| format!("clone output stream: {e}"))?;
    // Remote-close flag: set by the output thread when the stream ends
    // (job exit, daemon gone). The input loop polls stdin instead of
    // blocking in `read` so it notices the close WITHOUT a keypress —
    // a blocking read left the user staring at a cleared screen until
    // they hit Enter, whose bytes then hit the dead socket as a
    // "Broken pipe" error.
    let closed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let closed_flag = std::sync::Arc::clone(&closed);
    let output_thread = std::thread::Builder::new()
        .name("orkia-daemon-attach-output".to_string())
        .spawn(move || {
            copy_attach_output(&mut output);
            closed_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .map_err(|e| format!("spawn attach output thread: {e}"))?;

    let mut buf = [0_u8; 1024];
    let mut detached = false;
    let mut stdin_eof = false;
    loop {
        if closed.load(std::sync::atomic::Ordering::SeqCst) {
            // Job exited — the output thread already restored the tty.
            break;
        }
        match poll_stdin(100) {
            StdinPoll::Idle => continue,
            StdinPoll::Closed => {
                stdin_eof = true;
                break;
            }
            StdinPoll::Ready => {}
        }
        match forward_stdin_chunk(&mut stream, &mut buf) {
            Forward::Continue => {}
            Forward::Detached => {
                detached = true;
                break;
            }
            Forward::Closed => {
                stdin_eof = true;
                break;
            }
        }
    }
    if stdin_eof && !closed.load(std::sync::atomic::Ordering::SeqCst) {
        // Piped/scripted attach (stdin is not a tty): half-close so the
        // catch-up paint and any in-flight output still drain. A full
        // shutdown here raced the replay and returned an empty screen.
        // The drain is bounded: the runtime ends the attach session when
        // its input leg sees this EOF and closes the stream, which ends
        // the output thread below.
        let _ = stream.shutdown(std::net::Shutdown::Write);
    } else {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    let _ = output_thread.join();
    if detached {
        emit_cleanup_to_stdout();
    }
    Ok(())
}

enum Forward {
    Continue,
    Detached,
    Closed,
}

/// Read one stdin chunk and forward it to the attach stream, scanning
/// for the detach trigger. EOF, EPIPE on write (job-exit race), and
/// non-EINTR read errors all end the splice as `Closed` — never an
/// error surfaced to the user.
fn forward_stdin_chunk(stream: &mut UnixStream, buf: &mut [u8]) -> Forward {
    // SAFETY: buf is valid for buf.len() bytes; STDIN_FILENO is fd 0.
    let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            return Forward::Continue;
        }
        return Forward::Closed;
    }
    let n = n as usize;
    if n == 0 {
        return Forward::Closed;
    }
    if let Some((pos, _len)) = detach_match(&buf[..n]) {
        if pos > 0 {
            let _ = stream.write_all(&buf[..pos]);
        }
        return Forward::Detached;
    }
    if stream.write_all(&buf[..n]).is_err() {
        return Forward::Closed;
    }
    Forward::Continue
}

enum StdinPoll {
    Ready,
    Idle,
    Closed,
}

/// `poll(2)` stdin with a bounded timeout so the input loop can re-check
/// the remote-close flag between keystrokes. Mirrors raw_attach.rs
/// `run_input_loop`.
fn poll_stdin(timeout_ms: i32) -> StdinPoll {
    let mut pfd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: pfd is a valid pollfd; count=1 matches; timeout is a small
    // non-negative int.
    let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if ret <= 0 {
        // Timeout, EINTR, or transient failure — treat as idle so the
        // loop re-checks the close flag rather than dying on a signal.
        return StdinPoll::Idle;
    }
    if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
        return StdinPoll::Closed;
    }
    if pfd.revents & libc::POLLIN != 0 {
        StdinPoll::Ready
    } else {
        StdinPoll::Idle
    }
}

/// Emit detach/exit cleanup sequences to the local stdout so the host
/// terminal is restored after a vim/claude session. Matches the exact
/// sequence in raw_attach.rs `emit_detach_cleanup()`.
fn emit_cleanup_to_stdout() {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(DETACH_CLEANUP);
    let _ = out.flush();
}

fn copy_attach_output(output: &mut UnixStream) {
    let mut stdout = std::io::stdout().lock();
    let mut buf = [0_u8; 8192];
    loop {
        match output.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = stdout.flush();
            }
            Err(_) => break,
        }
    }
    // Stream closed = job exited or daemon disconnected. Restore host
    // terminal state so the next prompt renders cleanly.
    let _ = stdout.write_all(DETACH_CLEANUP);
    let _ = stdout.flush();
}

pub(super) fn pump(
    mut stream: UnixStream,
    history: Vec<u8>,
    snapshot: Vec<u8>,
    rx: mpsc::Receiver<Vec<u8>>,
    writer: orkia_terminal_core::SharedWriter,
) {
    let mut output = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let output_thread = std::thread::spawn(move || {
        let catch_up = if history.is_empty() {
            snapshot
        } else {
            history
        };
        if !catch_up.is_empty() {
            if output.write_all(&catch_up).is_err() {
                return;
            }
            let _ = output.flush();
        }
        loop {
            if stop_rx.try_recv().is_ok() {
                break;
            }
            match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(chunk) => {
                    if output.write_all(&chunk).is_err() {
                        break;
                    }
                    let _ = output.flush();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // Job PTY closed — emit cleanup so the client's host
                    // terminal is restored before the daemon-side stream closes.
                    let _ = output.write_all(DETACH_CLEANUP);
                    let _ = output.flush();
                    break;
                }
            }
        }
    });
    splice_input_to_writer(&mut stream, writer);
    let _ = stop_tx.send(());
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = output_thread.join();
}

fn splice_input_to_writer(stream: &mut UnixStream, writer: orkia_terminal_core::SharedWriter) {
    let mut buf = [0_u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                // Blocking lock — `try_lock` silently DROPPED the bytes just
                // read whenever the engine momentarily held the writer, losing
                // user keystrokes. The runtime-side twin
                // (`detached_control::splice input leg`) already blocks.
                let mut w = writer.lock();
                if w.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = w.flush();
            }
            Err(_) => break,
        }
    }
}

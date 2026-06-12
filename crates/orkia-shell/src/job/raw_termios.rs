// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Terminal raw mode via `termios(3)` directly.
//!
//! Used by [`super::raw_attach::run_raw_attach`]. **crossterm is
//! intentionally not used here** — its `enable_raw_mode()` writes
//! terminal-feature-negotiation escape sequences (bracketed paste,
//! focus events, kitty keyboard) that provoke iTerm to reply with DCS
//! capability strings (`\x1bP>|iTerm2 3.6.10\x1b\\`). Those reply
//! bytes land in our PTY pump and get forwarded to the attached child
//! as garbage keystrokes that corrupt its input state.
//!
//! By going straight to `tcgetattr` / `cfmakeraw` / `tcsetattr` we
//! avoid those side-effecting writes entirely. The shell continues to
//! use crossterm for its own TUI / prompt rendering — only the attach
//! pump uses this path.
//!
//! `cfmakeraw` clears all of `BRKINT IGNBRK PARMRK ISTRIP INLCR IGNCR
//! ICRNL IXON OPOST ECHO ECHONL ICANON ISIG IEXTEN` — exactly the
//! flags that need to be off for byte-perfect splicing. Notably it
//! turns off `ISIG` so a Ctrl-C byte (`0x03`) is delivered as a byte
//! to our `read(2)` rather than being eaten by the kernel as
//! `SIGINT`; we then forward that byte verbatim to the child PTY.

use std::io;
use std::os::fd::RawFd;
use std::sync::Mutex;

const STDIN_FD: RawFd = libc::STDIN_FILENO;

/// The original termios captured at [`enter_raw_mode`] time. Kept in a
/// global so the matching [`restore_terminal`] (or a Drop guard the
/// caller wraps around the call) can put the tty back. `Option` so
/// nested entries are a no-op rather than corrupting the saved state.
static ORIGINAL_TERMIOS: Mutex<Option<libc::termios>> = Mutex::new(None);

/// Put STDIN into raw mode and remember the prior termios so it can be
/// restored. Idempotent: a second call while raw mode is already
/// active leaves the saved state alone and returns `Ok(())`.
pub fn enter_raw_mode() -> io::Result<()> {
    let mut slot = ORIGINAL_TERMIOS
        .lock()
        .map_err(|_| io::Error::other("raw_termios state lock poisoned"))?;

    // Already in raw mode — second entry without a restore is benign,
    // most likely a re-attach. Don't overwrite the snapshot.
    if slot.is_some() {
        return Ok(());
    }

    // SAFETY: STDIN_FD is a valid OS fd for every Unix process; we
    // write into a stack-allocated termios; tcgetattr returns -1 on
    // failure which we map to a Rust io error.
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::tcgetattr(STDIN_FD, &mut termios) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    let original = termios;
    unsafe { libc::cfmakeraw(&mut termios) };

    let rc = unsafe { libc::tcsetattr(STDIN_FD, libc::TCSANOW, &termios) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    *slot = Some(original);
    Ok(())
}

/// Restore the prior termios captured by [`enter_raw_mode`]. Safe to
/// call when raw mode was never entered (no-op).
pub fn restore_terminal() -> io::Result<()> {
    let original = {
        let mut slot = ORIGINAL_TERMIOS
            .lock()
            .map_err(|_| io::Error::other("raw_termios state lock poisoned"))?;
        slot.take()
    };

    if let Some(termios) = original {
        let rc = unsafe { libc::tcsetattr(STDIN_FD, libc::TCSANOW, &termios) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// RAII guard that enters raw mode on construction and restores on
/// drop. Use this in any caller path where a panic / early return
/// must not leave the terminal in raw mode.
pub struct RawModeGuard {
    _no_send: std::marker::PhantomData<*const ()>,
}

impl RawModeGuard {
    pub fn enter() -> io::Result<Self> {
        enter_raw_mode()?;
        Ok(Self {
            _no_send: std::marker::PhantomData,
        })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = restore_terminal();
    }
}

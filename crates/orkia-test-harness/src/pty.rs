// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! PTY driver — wraps a `portable-pty` master so tests can type
//! keystrokes, read raw bytes, and query a parsed screen grid.
//!
//! Two threads are spawned per PTY:
//!   * a reader thread that drains the master into both a raw byte
//!     ring (for "did we see this byte" assertions) and a `vt100`
//!     parser (for "does the grid contain this text" assertions);
//!   * the master writer is kept on the main task (call `type_bytes`).
//!
//! The grid uses `vt100::Parser` because it is dev-dep light and
//! sufficient for assertion needs (text scraping, cursor position).
//! Production code uses `alacritty_terminal`; both follow the same
//! VT500 series semantics for the assertions we make.

use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

use crate::wait::{WaitError, wait_for};

/// Tunable shape passed to `portable_pty::native_pty_system().openpty`.
#[derive(Clone, Copy, Debug)]
pub struct PtyShape {
    pub rows: u16,
    pub cols: u16,
}

impl Default for PtyShape {
    fn default() -> Self {
        Self {
            rows: 40,
            cols: 120,
        }
    }
}

/// Owns one PTY master, the child process attached to its slave, and
/// the background reader. Cloneable handles (`raw_buffer`, `grid`)
/// remain valid for the lifetime of the driver.
pub struct PtyDriver {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn std::io::Write + Send>,
    raw: Arc<Mutex<Vec<u8>>>,
    parser: Arc<Mutex<vt100::Parser>>,
    shape: PtyShape,
}

impl PtyDriver {
    /// Spawn `cmd` inside a fresh PTY. The reader thread starts
    /// immediately.
    pub fn spawn(cmd: CommandBuilder, shape: PtyShape) -> anyhow::Result<Self> {
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: shape.rows,
            cols: shape.cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let child = pair.slave.spawn_command(cmd)?;
        let writer = pair.master.take_writer()?;
        let raw = Arc::new(Mutex::new(Vec::<u8>::with_capacity(64 * 1024)));
        let parser = Arc::new(Mutex::new(vt100::Parser::new(shape.rows, shape.cols, 4096)));
        let mut reader = pair.master.try_clone_reader()?;
        let raw_bg = raw.clone();
        let parser_bg = parser.clone();
        std::thread::Builder::new()
            .name("orkia-test-harness-pty-reader".into())
            .spawn(move || {
                let mut buf = [0u8; 8 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let chunk = &buf[..n];
                            if let Ok(mut g) = raw_bg.lock() {
                                g.extend_from_slice(chunk);
                            }
                            if let Ok(mut p) = parser_bg.lock() {
                                p.process(chunk);
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            })?;
        Ok(Self {
            master: pair.master,
            child,
            writer,
            raw,
            parser,
            shape,
        })
    }

    /// Write raw bytes to the master (the child sees them on stdin).
    /// Use this for control bytes (e.g. `&[0x03]` for Ctrl-C).
    pub fn write(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        use std::io::Write;
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Write a string then a carriage-return (what a real terminal
    /// sends when the user presses Enter — crossterm/rustyline-driven
    /// readers expect CR, not LF).
    ///
    /// Bytes are dripped one at a time with a small inter-byte delay.
    /// rustyline (which `orkia`'s shell-mode renderer uses) silently
    /// drops bytes when they arrive in a single PTY chunk — only the
    /// first character per chunk gets through. Real users type slowly
    /// enough that this never matters; PTY-driven tests have to mimic
    /// that cadence.
    pub fn type_line(&mut self, line: &str) -> anyhow::Result<()> {
        self.type_str(line)?;
        self.write(b"\r")?;
        Ok(())
    }

    /// Send a string with no implicit newline.
    ///
    /// Bytes are dripped one at a time — see [`Self::type_line`] for
    /// the rationale.
    pub fn type_str(&mut self, s: &str) -> anyhow::Result<()> {
        for chunk in s.as_bytes().chunks(1) {
            self.write(chunk)?;
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    /// Resize the PTY. Forwarded to the child as SIGWINCH automatically.
    pub fn resize(&mut self, rows: u16, cols: u16) -> anyhow::Result<()> {
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        if let Ok(mut p) = self.parser.lock() {
            p.set_size(rows, cols);
        }
        self.shape = PtyShape { rows, cols };
        Ok(())
    }

    /// All raw bytes the child has emitted so far.
    pub fn raw_bytes(&self) -> Vec<u8> {
        self.raw.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// All raw bytes decoded lossily as UTF-8. Useful for substring
    /// assertions when you don't care about exact byte sequences.
    pub fn raw_text(&self) -> String {
        let bytes = self.raw_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Snapshot every visible row of the parsed grid as a `Vec<String>`.
    pub fn screen_lines(&self) -> Vec<String> {
        let p = match self.parser.lock() {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        };
        let screen = p.screen();
        (0..self.shape.rows)
            .map(|r| {
                (0..self.shape.cols)
                    .map(|c| {
                        screen
                            .cell(r, c)
                            .map(|cell| cell.contents())
                            .unwrap_or_default()
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Single-string visible screen with newlines between rows.
    pub fn screen_text(&self) -> String {
        self.screen_lines().join("\n")
    }

    /// Wait for `needle` to appear in either the parsed screen or the
    /// raw byte stream (whichever the caller's content is more likely
    /// to be visible in). Returns the matching snapshot.
    pub async fn wait_for_text(
        &self,
        needle: &str,
        timeout: Duration,
    ) -> Result<String, WaitError> {
        let needle_owned = needle.to_string();
        wait_for(
            timeout,
            Duration::from_millis(25),
            || async {
                let screen = self.screen_text();
                if screen.contains(&needle_owned) {
                    return Some(screen);
                }
                let raw = self.raw_text();
                if raw.contains(&needle_owned) {
                    return Some(raw);
                }
                None
            },
            || format!("waiting for PTY text: {needle:?}"),
        )
        .await
    }

    /// Wait for `needle` in the CURRENT parsed screen grid only — not
    /// the accumulated raw byte stream. Use this when a regression would
    /// hide behind history: e.g. asserting that a re-attach actually
    /// repaints the screen. `raw_text` still holds bytes from a prior
    /// attach, so [`Self::wait_for_text`] would false-pass; the live
    /// grid reflects only what is on screen right now.
    pub async fn wait_for_screen_text(
        &self,
        needle: &str,
        timeout: Duration,
    ) -> Result<String, WaitError> {
        let needle_owned = needle.to_string();
        wait_for(
            timeout,
            Duration::from_millis(25),
            || async {
                let screen = self.screen_text();
                screen.contains(&needle_owned).then_some(screen)
            },
            || format!("waiting for screen text: {needle:?}"),
        )
        .await
    }

    /// Wait for the child to exit. Returns the exit status.
    pub fn wait_exit(
        &mut self,
        timeout: Duration,
    ) -> Result<portable_pty::ExitStatus, anyhow::Error> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("child did not exit within {timeout:?}");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// SIGKILL the child. Idempotent.
    pub fn kill(&mut self) -> anyhow::Result<()> {
        let _ = self.child.kill();
        Ok(())
    }

    pub fn shape(&self) -> PtyShape {
        self.shape
    }
}

impl Drop for PtyDriver {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

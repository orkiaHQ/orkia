// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! PTY abstraction: opens a hidden interactive shell behind a pseudo-terminal
//! with OSC-133 prompt hooks, and exposes the raw read/write/resize handles.
//! Contains no terminal-engine logic (block parsing, snapshotting) and no
//! user-facing strings — only typed errors and `tracing` diagnostics. The
//! reader thread that drives the engine lives in `orkia-terminal-core`.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::Arc;

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Config;
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

pub type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;
pub type ScreenTerm = Arc<FairMutex<Term<EventProxy>>>;
pub type SharedMaster = Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>;
/// Live (cols, rows) — updated as the pane resizes.
pub type SharedDims = Arc<Mutex<(usize, usize)>>;

/// Errors surfaced by this crate. Programmatic variants — never shown to a
/// user (the application crate maps these to localized messages).
///
/// `Backend` previously collapsed every failure into an opaque `String`
/// (audit P3-011); finer variants now preserve enough information for
/// downstream layers to branch on cause (e.g. missing-pid vs spawn-fail
/// vs writer-init-fail) without round-tripping through string parsing.
/// `Backend` is kept for genuinely opaque `portable-pty` errors that
/// don't fit any of the other variants.
#[derive(thiserror::Error, Debug)]
pub enum PtyError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The child handle does not expose a pid (already reaped, or the
    /// backend never assigned one). Programmatic; never user-facing.
    #[error("pty: child has no pid")]
    NoPid,
    /// Caller invoked an entry point that requires an explicit command
    /// but didn't provide one (currently `spawn_config` with `cmd = None`).
    #[error("pty: spawn_config requires an explicit command; use open_pair for brush embedding")]
    MissingCommand,
    /// `portable-pty` reported an opaque error during `openpty`,
    /// `spawn_command`, `take_writer`, or `try_clone_reader`. The source
    /// chain is rendered as a single string at the boundary — the
    /// underlying `anyhow::Error` is not exposed to keep the crate's
    /// public type surface stable.
    #[error("pty backend: {0}")]
    Backend(String),
    /// `portable-pty` reported a `try_wait` failure. Distinct from
    /// `Backend` so callers can branch on "child is unreapable" without
    /// substring-matching the error message.
    #[error("pty: try_wait failed: {0}")]
    Wait(String),
}

/// Fixed grid geometry helper for the display-only screen `Term`.
#[derive(Clone, Copy)]
pub struct Dims {
    pub cols: usize,
    pub rows: usize,
}
impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Forwards `PtyWrite` (DSR/DA query responses) so TUIs don't hang.
///
/// alacritty calls `send_event` synchronously from inside the reader
/// thread's parser. If we wrote through the master fd inline under the
/// shared writer lock, a stuck PTY (kernel write queue full because the
/// child is wedged) would block the reader's parser indefinitely. To
/// keep the parser hot, replies are pushed onto a non-blocking queue
/// drained by a dedicated `orkia-pty-reply-writer` thread that owns
/// the actual `writer.lock()` call. The reader thread never blocks on
/// disk / kernel write queues (audit P3-009).
#[derive(Clone)]
pub struct EventProxy {
    reply_tx: std::sync::mpsc::SyncSender<Vec<u8>>,
}

/// Hard cap on queued reply bytes. DSR/DA responses are 10–40 bytes and
/// fire a handful of times per second at most; 1024 slots is generous
/// enough to absorb a burst without backing up the parser.
const PTY_REPLY_QUEUE_BOUND: usize = 1024;

impl EventProxy {
    fn spawn(writer: SharedWriter) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(PTY_REPLY_QUEUE_BOUND);
        let spawn_result = std::thread::Builder::new()
            .name("orkia-pty-reply-writer".into())
            .spawn(move || pty_reply_loop(writer, rx));
        if let Err(err) = spawn_result {
            tracing::error!(
                ?err,
                "EventProxy: reply-writer thread spawn failed; DSR/DA \
                replies will be dropped this session",
            );
        }
        Self { reply_tx: tx }
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            match self.reply_tx.try_send(text.into_bytes()) {
                Ok(()) => {}
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    // Writer thread has exited (process shutdown). Replies
                    // are no longer relevant; silently drop.
                }
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    tracing::warn!("EventProxy: reply queue full — dropping PtyWrite response",);
                }
            }
        }
    }
}

fn pty_reply_loop(writer: SharedWriter, rx: std::sync::mpsc::Receiver<Vec<u8>>) {
    while let Ok(bytes) = rx.recv() {
        let mut w = writer.lock();
        if w.write_all(&bytes).is_err() {
            // PTY closed (child gone). Drain and exit so the channel
            // doesn't accumulate replies that will never be delivered.
            drop(w);
            while rx.recv().is_ok() {}
            return;
        }
        let _ = w.flush();
    }
}

/// Resize the PTY + live screen `Term` + shared dims (called on pane resize).
///
/// Holds the `dims` lock for the duration of the master + screen
/// resizes so a concurrent observer cannot see the new dims while the
/// underlying geometry is still old (audit P3-012). The dims lock is
/// always taken first, never nested inside the master or screen locks
/// elsewhere in the crate, so this ordering remains deadlock-free.
pub fn apply_resize(
    master: &SharedMaster,
    screen: &ScreenTerm,
    dims: &SharedDims,
    cols: usize,
    rows: usize,
) {
    let mut d = dims.lock();
    if *d == (cols, rows) {
        return;
    }
    let _ = master.lock().resize(PtySize {
        rows: rows as u16,
        cols: cols as u16,
        pixel_width: 0,
        pixel_height: 0,
    });
    screen.lock().resize(Dims { cols, rows });
    *d = (cols, rows);
}

/// Configuration for spawning a PTY process.
pub struct SpawnConfig {
    pub cols: usize,
    pub rows: usize,
    /// Command to run. Must be `Some(_)` for `spawn_config`.
    pub cmd: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Extra environment variables. Applied after parent env inheritance so
    /// they take precedence over any inherited value with the same key.
    pub env: Vec<(String, String)>,
}

impl SpawnConfig {
    /// Build a config for spawning an arbitrary command in a PTY (no
    /// OSC-133 injection). Used by `TerminalEngine::start` to launch
    /// agent jobs. Shell commands no longer go through here — they run
    /// in-process via `brush_core::Shell`.
    pub fn command(cmd: impl Into<String>, args: Vec<String>, cols: usize, rows: usize) -> Self {
        Self {
            cols,
            rows,
            cmd: Some(cmd.into()),
            args,
            cwd: None,
            env: Vec::new(),
        }
    }
}

/// Raw PTY handles. The engine (in `orkia-terminal-core`) consumes these:
/// it owns the reader thread, never this crate.
pub struct PtyProcess {
    pub writer: SharedWriter,
    pub reader: Box<dyn Read + Send>,
    pub master: SharedMaster,
    pub dims: SharedDims,
    pub screen: ScreenTerm,
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl PtyProcess {
    pub fn child_id(&self) -> Option<u32> {
        self.child.process_id()
    }

    #[cfg(unix)]
    pub fn send_signal(&mut self, sig: i32) -> Result<(), PtyError> {
        // Guard: if the child has already exited, its pid may have been
        // recycled by the kernel. Sending a signal to a recycled pid could
        // hit an unrelated process. Check via try_wait (non-blocking) and
        // return an error rather than calling kill(2) on a dead child.
        if let Some(_exit) = self.try_wait()? {
            return Err(PtyError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "send_signal: child already exited",
            )));
        }
        let pid = self.child.process_id().ok_or(PtyError::NoPid)?;
        // SAFETY: `kill(2)` with a pid from our own child handle. We are
        // the sole owner of `self.child` and we just confirmed via
        // try_wait (above) that the child has not yet been reaped, so the
        // pid is stable. The signal number is validated by the kernel; an
        // invalid `sig` returns `EINVAL` rather than producing UB.
        let ret = unsafe { libc::kill(pid as libc::pid_t, sig) };
        if ret == 0 {
            Ok(())
        } else {
            Err(PtyError::Io(std::io::Error::last_os_error()))
        }
    }

    pub fn try_wait(&mut self) -> Result<Option<i32>, PtyError> {
        match self.child.try_wait() {
            Ok(Some(status)) => Ok(Some(status.exit_code().try_into().unwrap_or(-1))),
            Ok(None) => Ok(None),
            Err(e) => Err(PtyError::Wait(e.to_string())),
        }
    }
}

fn backend<E: std::fmt::Display>(e: E) -> PtyError {
    PtyError::Backend(e.to_string())
}

/// Open a PTY with the given configuration and return the raw handles.
/// `cfg.cmd` must be `Some(_)` — this entry point exists for agent jobs;
/// for embedding brush in-process use [`open_pair`] instead.
pub fn spawn_config(cfg: SpawnConfig) -> Result<PtyProcess, PtyError> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: cfg.rows as u16,
            cols: cfg.cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(backend)?;

    let program = cfg.cmd.as_deref().ok_or(PtyError::MissingCommand)?;
    let mut cmd_builder = CommandBuilder::new(program);
    for arg in &cfg.args {
        cmd_builder.arg(arg);
    }
    for (k, v) in std::env::vars() {
        cmd_builder.env(k, v);
    }
    cmd_builder.env("TERM", "xterm-256color");
    cmd_builder.env("COLORTERM", "truecolor");
    cmd_builder.env("TERM_PROGRAM", "orkia-terminal");
    for (k, v) in &cfg.env {
        cmd_builder.env(k, v);
    }
    cmd_builder.cwd(
        cfg.cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))),
    );

    let child = pair.slave.spawn_command(cmd_builder).map_err(backend)?;
    drop(pair.slave);

    let writer: SharedWriter = Arc::new(Mutex::new(pair.master.take_writer().map_err(backend)?));
    let reader = pair.master.try_clone_reader().map_err(backend)?;
    let master: SharedMaster = Arc::new(Mutex::new(pair.master));
    let dims: SharedDims = Arc::new(Mutex::new((cfg.cols, cfg.rows)));
    let screen: ScreenTerm = Arc::new(FairMutex::new(Term::new(
        Config {
            scrolling_history: 0,
            ..Config::default()
        },
        &Dims {
            cols: cfg.cols,
            rows: cfg.rows,
        },
        EventProxy::spawn(Arc::clone(&writer)),
    )));

    tracing::debug!(cols = cfg.cols, rows = cfg.rows, "pty spawned");
    Ok(PtyProcess {
        writer,
        reader,
        master,
        dims,
        screen,
        child,
    })
}

/// Raw PTY pair for embedding a Rust-native shell engine (brush) in-process.
/// Unlike [`PtyProcess`], no child is spawned: the caller hands `slave` to a
/// shell engine via its `open_files` and reads/writes the master from the
/// hosting application.
pub struct AdoptedPty {
    /// Reader half of the master, for the terminal engine's reader thread.
    pub reader: Box<dyn Read + Send>,
    /// Writer half of the master, for the application to inject bytes
    /// (e.g. OSC-133 sequences emitted around each command).
    pub writer: SharedWriter,
    /// Master fd; kept alive for resize ioctls.
    pub master_fd: Arc<OwnedFd>,
    /// Slave fd; hand this to the embedded shell engine.
    pub slave: OwnedFd,
    pub dims: SharedDims,
    pub screen: ScreenTerm,
}

/// Open a raw PTY pair via `openpty(3)`. Sets initial window size and
/// CLOEXEC on both fds (so they aren't leaked into accidentally inherited
/// child processes).
pub fn open_pair(cols: usize, rows: usize) -> Result<AdoptedPty, PtyError> {
    let mut ws = libc::winsize {
        ws_row: rows as libc::c_ushort,
        ws_col: cols as libc::c_ushort,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: openpty writes two fds via out-pointers; name/termios are NULL.
    // libc's signature takes *mut for ws (Apple) but doesn't write to it.
    // `&mut ws` satisfies both signatures (Apple's `*mut`, Linux's `*const`);
    // on Linux clippy flags the `&mut` as unnecessary, but dropping it would
    // fail to compile on macOS — so the lint is allowed here, not the code
    // changed (justified per CLAUDE.md no-bare-allow rule).
    #[allow(clippy::unnecessary_mut_passed)]
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut ws,
        )
    };
    if rc != 0 {
        return Err(PtyError::Io(std::io::Error::last_os_error()));
    }
    // Set FD_CLOEXEC on master so it doesn't leak into brush's children.
    // The slave intentionally stays inheritable: brush's children inherit it.
    // SAFETY: `master` was returned by `openpty` above and is still owned
    // here (we haven't yet wrapped it in `OwnedFd`). fcntl(F_GETFD/F_SETFD)
    // does not transfer ownership or invalidate the descriptor.
    unsafe {
        let flags = libc::fcntl(master, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(master, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
    // SAFETY: openpty returned valid fds; we have exclusive ownership.
    let master_fd = unsafe { OwnedFd::from_raw_fd(master) };
    let slave_fd = unsafe { OwnedFd::from_raw_fd(slave) };

    let master_for_reader = master_fd.try_clone().map_err(PtyError::Io)?;
    let master_for_writer = master_fd.try_clone().map_err(PtyError::Io)?;
    let reader: Box<dyn Read + Send> = Box::new(std::fs::File::from(master_for_reader));
    let writer: SharedWriter =
        Arc::new(Mutex::new(Box::new(std::fs::File::from(master_for_writer))));

    let dims: SharedDims = Arc::new(Mutex::new((cols, rows)));
    let screen: ScreenTerm = Arc::new(FairMutex::new(Term::new(
        Config {
            scrolling_history: 0,
            ..Config::default()
        },
        &Dims { cols, rows },
        EventProxy::spawn(Arc::clone(&writer)),
    )));

    tracing::debug!(cols, rows, "adopted pty pair opened");
    Ok(AdoptedPty {
        reader,
        writer,
        master_fd: Arc::new(master_fd),
        slave: slave_fd,
        dims,
        screen,
    })
}

/// Resize an adopted PTY (TIOCSWINSZ ioctl on the master fd). Also resizes
/// the screen Term and updates shared dims.
///
/// Holds the `dims` lock across the ioctl + screen-resize for the same
/// reason as [`apply_resize`] (audit P3-012). On ioctl failure the dims
/// are left unchanged so subsequent observers don't see a phantom
/// resize.
pub fn resize_adopted(
    master_fd: &OwnedFd,
    screen: &ScreenTerm,
    dims: &SharedDims,
    cols: usize,
    rows: usize,
) -> Result<(), PtyError> {
    let mut d = dims.lock();
    if *d == (cols, rows) {
        return Ok(());
    }
    let ws = libc::winsize {
        ws_row: rows as libc::c_ushort,
        ws_col: cols as libc::c_ushort,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ on a master pty fd is the standard resize ioctl.
    let rc = unsafe { libc::ioctl(master_fd.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
    if rc != 0 {
        return Err(PtyError::Io(std::io::Error::last_os_error()));
    }
    screen.lock().resize(Dims { cols, rows });
    *d = (cols, rows);
    Ok(())
}

// NOTE: the prior `fallback_command_set` / `shell_command_set` / zsh-probe
// machinery is removed. The classifier no longer consults an external
// command vocabulary — brush itself returns 127 for unknown commands,
// which is the correct UX with no startup latency or external-shell
// dependency.

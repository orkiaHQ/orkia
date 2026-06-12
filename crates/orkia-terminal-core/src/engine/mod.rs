// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `TerminalEngine` — the validated three-thread lock-free model, packaged as
//! a black box the application consumes. It owns the PTY (via `orkia-pty`),
//! the reader thread (drains the PTY, advances the grid), and the extractor
//! thread (publishes immutable snapshots). The render path clones a published
//! `Arc`. See `ARCHITECTURE-TERMINAL.md`.
//!
//! The reader-thread loop below is the validated hot path, relocated verbatim
//! from the POC `spawn_shell`. Do not move work between threads or change the
//! synchronization here without re-running `bench-terminal`.

mod reader;

use std::io::Read;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;

use orkia_pty::{ScreenTerm, SharedDims, SharedMaster, SharedWriter};
use parking_lot::Mutex;

use crate::blocks::{BlockParser, BlocksState, SharedBlocks, spawn_extractor};
use crate::config::EngineConfig;
use crate::error::EngineError;
use crate::screen_view::{ScreenPublisher, ScreenView};
use crate::state::{SharedState, StateMachine};
use crate::wake::{Wake, WakeRx, wake_pair};

use reader::{HistoryRing, ReaderCtx, run_reader_loop};

pub type RawOutputRx = mpsc::Receiver<Vec<u8>>;
pub(super) type RawOutputTx = mpsc::SyncSender<Vec<u8>>;
pub(super) type SubscriberTx = mpsc::SyncSender<Vec<u8>>;

/// Bound on the raw PTY byte channel between the reader thread and the
/// attach pump's stdout drain. Each slot holds a `Vec<u8>` of up to
/// `EngineConfig::read_buf_bytes` (default 8 KiB). At 4096 slots the
/// worst-case backlog is ≈ 32 MiB — large enough to absorb a multi-screen
/// burst from a chatty agent without OOM but small enough to apply
/// back-pressure to the reader if the consumer wedges.
const RAW_OUTPUT_BOUND: usize = 4096;

/// Same shape for the per-subscriber fan-out channels (prompt detector,
/// state-machine input scanner, etc.). Subscribers are passive observers
/// — if they fall behind the GC drops the sender so the reader doesn't
/// stall the whole pipeline; until then the bound caps memory usage.
const SUBSCRIBER_BOUND: usize = 1024;

/// Master-side PTY handles for [`TerminalEngine::adopt_master`]. Bundled
/// into a struct so the engine's constructor stays under the 4-argument
/// limit and so callers can populate via `AdoptMaster { .. }` syntax.
pub struct AdoptMaster {
    pub reader: Box<dyn Read + Send>,
    pub writer: SharedWriter,
    pub master_fd: Arc<OwnedFd>,
    pub dims: SharedDims,
    pub screen: ScreenTerm,
    /// Reader-thread read-buffer size. Sensible default is 64 KiB.
    pub buf_bytes: usize,
    /// Optional OSC 133 marker listener — see
    /// [`EngineConfig::on_osc133`].
    pub on_osc133: Option<crate::blocks::Osc133Callback>,
    /// Optional APC payload listener — see [`EngineConfig::on_apc`].
    pub on_apc: Option<crate::blocks::ApcCallback>,
}

/// Owns the PTY + the reader and extractor threads. Accessors hand the
/// application the shared handles it needs; `take_wake_rx` yields the single
/// repaint-consumer receiver exactly once.
pub(super) type SharedChild = Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>;

pub struct TerminalEngine {
    writer: SharedWriter,
    blocks: SharedBlocks,
    sm: SharedState,
    screen: ScreenTerm,
    /// Present only for engines spawned with a portable-pty child via
    /// [`Self::start`]. `adopt_master` engines (the brush session PTY)
    /// resize via [`Self::adopted_master_fd`] instead.
    master: Option<SharedMaster>,
    /// Present only for `adopt_master` engines. Used for `TIOCSWINSZ`.
    adopted_master_fd: Option<Arc<OwnedFd>>,
    dims: SharedDims,
    wake: Wake,
    wake_rx: Option<WakeRx>,
    /// Absent when the engine adopts an externally managed PTY (brush
    /// owns its own children, not us).
    child: Option<SharedChild>,
    raw_output_rx: Mutex<Option<RawOutputRx>>,
    /// Rolling ring buffer of recent PTY output bytes. The reader
    /// thread tees every chunk in here in addition to forwarding it
    /// down `raw_output_rx`. The attach pump dumps a snapshot of this
    /// buffer to stdout on attach so a re-attach reconstructs the
    /// visible terminal state without needing the child to redraw.
    /// 256 KiB is enough for a full-screen TUI redraw on typical
    /// dimensions (claude / vim / htop / less).
    history: Arc<Mutex<HistoryRing>>,
    /// Fan-out senders for passive observers (prompt detector, etc).
    /// The reader thread clones each chunk to every subscriber after
    /// dispatching to the primary `raw_tx` and history ring. Adding a
    /// subscriber mid-session only sees future chunks — history catch-up
    /// is the responsibility of the consumer (typically by snapshotting
    /// the ring once via [`Self::history_snapshot`]).
    subscribers: Arc<Mutex<Vec<SubscriberTx>>>,
    /// Lock-free read slot for the screen-mode `ScreenSnapshot`,
    /// published by the engine reader thread. The reader owns the
    /// publisher; the `Arc<ArcSwap>` is cloned into every `ScreenView`
    /// handed out via [`Self::screen_view`].
    screen_snapshot: Arc<arc_swap::ArcSwap<crate::screen_view::ScreenSnapshot>>,
    /// Set by the reader thread the instant the child's PTY hits EOF
    /// (process exited). Subscribers (the prompt detector) poll this to
    /// detect exit promptly — the subscriber channel itself does NOT
    /// disconnect on exit because the engine keeps its sender alive for
    /// late `subscribe_output` callers, so EOF is the only signal.
    child_exited: Arc<AtomicBool>,
    /// Exit code recorded by the reader thread's bounded reap at EOF,
    /// written before `child_exited` is set. `Some(n)` once reaped,
    /// `None` if the child wasn't reapable within the grace (the REPL's
    /// authoritative `try_wait` collects it later either way — the
    /// `std::process::Child` behind the `Child` trait caches the status,
    /// so this reader-side reap and the REPL reap never conflict).
    child_exit_code: Arc<Mutex<Option<i32>>>,
}

// ─── Constructors ────────────────────────────────────────────────────────────

impl TerminalEngine {
    pub fn start(cfg: EngineConfig) -> Result<Self, EngineError> {
        let pty = orkia_pty::spawn_config(spawn_cfg_from_engine(&cfg))?;
        let orkia_pty::PtyProcess {
            writer,
            reader,
            master,
            dims,
            screen,
            child,
        } = pty;
        // Own the child so the reader thread can reap its exit code at EOF.
        let child_arc: SharedChild = Arc::new(Mutex::new(child));

        let (engine, reader_ctx) = build_engine_and_ctx(BuildEngineCtx {
            writer,
            dims,
            screen,
            persistent_program: cfg.persistent_program,
            buf_bytes: cfg.read_buf_bytes,
            on_osc133: cfg.on_osc133.clone(),
            on_apc: cfg.on_apc.clone(),
            init_cols: cfg.init_cols,
            init_rows: cfg.init_rows,
            min_publish: cfg.screen_extract.min_publish,
            master: Some(master),
            adopted_master_fd: None,
            child: Some(Arc::clone(&child_arc)),
            batch_prescan: true,
        });

        let child_exit_code_reader = Arc::clone(&engine.child_exit_code);
        let child_exited_reader = Arc::clone(&engine.child_exited);
        std::thread::spawn(move || {
            run_reader_loop(ReaderCtx {
                reader: Box::new(reader),
                child_exit_code: Some(child_exit_code_reader),
                child_reaper: Some(child_arc),
                child_exited: child_exited_reader,
                ..reader_ctx
            });
        });

        tracing::info!("terminal engine started");
        Ok(engine)
    }

    /// Adopt the master half of a raw PTY pair (the slave half is handed
    /// to brush by the caller via [`orkia_shell::engine::pty::bind_pty_to_shell`]).
    /// Same 3-thread plumbing as [`Self::start`] minus the child-management
    /// half (brush manages its own children).
    pub fn adopt_master(parts: AdoptMaster) -> Result<Self, EngineError> {
        let AdoptMaster {
            reader,
            writer,
            master_fd,
            dims,
            screen,
            buf_bytes,
            on_osc133,
            on_apc,
        } = parts;

        // Pull initial dims for the ScreenPublisher seed.
        let (seed_cols, seed_rows) = *dims.lock();

        let (engine, reader_ctx) = build_engine_and_ctx(BuildEngineCtx {
            writer,
            dims,
            screen,
            persistent_program: false,
            buf_bytes,
            on_osc133,
            on_apc,
            init_cols: seed_cols,
            init_rows: seed_rows,
            // `adopt_master` predates a dedicated `EngineConfig` route;
            // use the same default as `start`. PR3+ may surface this on
            // `AdoptMaster` if needed.
            min_publish: crate::config::ScreenExtractConfig::default().min_publish,
            master: None,
            adopted_master_fd: Some(master_fd),
            child: None,
            batch_prescan: false,
        });

        let child_exited_reader = Arc::clone(&engine.child_exited);

        std::thread::spawn(move || {
            run_reader_loop(ReaderCtx {
                reader,
                child_exit_code: None,
                child_reaper: None,
                child_exited: child_exited_reader,
                ..reader_ctx
            });
        });

        tracing::info!("terminal engine adopted master");
        Ok(engine)
    }
}

// ─── Accessors and utilities ─────────────────────────────────────────────────

impl TerminalEngine {
    /// Resize the underlying PTY + screen Term + shared dims. Works for
    /// both `start` and `adopt_master` engines.
    pub fn resize(&self, cols: usize, rows: usize) -> Result<(), EngineError> {
        if let Some(master) = self.master.as_ref() {
            orkia_pty::apply_resize(master, &self.screen, &self.dims, cols, rows);
            Ok(())
        } else if let Some(fd) = self.adopted_master_fd.as_ref() {
            orkia_pty::resize_adopted(fd, &self.screen, &self.dims, cols, rows)
                .map_err(EngineError::Pty)
        } else {
            Ok(())
        }
    }

    pub fn writer(&self) -> SharedWriter {
        Arc::clone(&self.writer)
    }
    pub fn blocks(&self) -> SharedBlocks {
        Arc::clone(&self.blocks)
    }
    pub fn state(&self) -> SharedState {
        Arc::clone(&self.sm)
    }
    pub fn screen(&self) -> ScreenTerm {
        Arc::clone(&self.screen)
    }
    /// Lock-free read handle on the latest published `ScreenSnapshot`.
    /// Cheap to clone (two `Arc`s). Multiple consumers may hold one;
    /// they observe a coherent snapshot per `load()` without
    /// coordinating with each other or with the reader thread.
    ///
    /// PR1 of `pty-actor-refactor`: this accessor is **additive**.
    /// Existing callers continue to use [`Self::screen`] /
    /// [`Self::render_visible_snapshot`].
    pub fn screen_view(&self) -> ScreenView {
        ScreenView::new(Arc::clone(&self.screen_snapshot), self.wake.clone())
    }
    /// portable-pty master (only for [`Self::start`] engines).
    /// Adopted engines return `None`; use [`Self::resize`] instead.
    pub fn master(&self) -> Option<SharedMaster> {
        self.master.as_ref().map(Arc::clone)
    }
    pub fn dims(&self) -> SharedDims {
        Arc::clone(&self.dims)
    }
    pub fn wake(&self) -> Wake {
        self.wake.clone()
    }
    /// The repaint consumer. Exactly one consumer exists; returns `None` if
    /// already taken.
    pub fn take_wake_rx(&mut self) -> Option<WakeRx> {
        self.wake_rx.take()
    }

    /// Take the raw PTY output receiver. Exactly one consumer at a
    /// time; returns `None` if already taken. Bytes arrive as they are
    /// read from the PTY.
    pub fn take_raw_output_rx(&self) -> Option<RawOutputRx> {
        self.raw_output_rx.lock().take()
    }

    /// Snapshot the ring buffer of recent PTY output bytes. Used by
    /// the attach pump to replay history on (re-)attach so the user
    /// sees the current visual state without needing the child to
    /// emit a fresh redraw.
    pub fn history_snapshot(&self) -> Vec<u8> {
        self.history.lock().snapshot()
    }

    /// Render the currently-visible alacritty grid back to ANSI
    /// bytes. Preferred over [`Self::history_snapshot`] for the
    /// attach pump: history replay can show stacked duplicate UIs
    /// when the child redrew itself in the byte stream (claude
    /// does this around prompt injection), while grid rendering
    /// always shows exactly one screen — the *current visible
    /// state* — regardless of how the bytes got there. See
    /// `render_snapshot.rs` for the renderer details.
    pub fn render_visible_snapshot(&self) -> Vec<u8> {
        let term = self.screen.lock();
        crate::render_snapshot::render_visible(&*term).into_bytes()
    }

    /// Subscribe a passive observer to the live PTY output stream.
    /// Every byte chunk the reader thread receives after this call is
    /// also delivered down the returned receiver. Drop the receiver to
    /// unsubscribe — the next fan-out call will GC the dead sender.
    ///
    /// Subscribers DO NOT see chunks that arrived before subscription;
    /// pair with [`Self::history_snapshot`] if you need catch-up.
    pub fn subscribe_output(&self) -> mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(SUBSCRIBER_BOUND);
        self.subscribers.lock().push(tx);
        rx
    }

    /// Hand out the child-exited flag — set by the reader thread the
    /// instant the PTY hits EOF (process exit). A passive observer (the
    /// prompt detector) polls it to learn of exit promptly, since the
    /// subscriber channel does not disconnect on exit.
    pub fn child_exited_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.child_exited)
    }

    /// The exit code recorded by the reader's bounded reap at EOF. Read it
    /// together with [`Self::child_exited_handle`]: once that flag is set,
    /// this is `Some(code)` if the child was reapable in the grace window,
    /// else `None`. Lets the prompt-exit notice be exact (`Done`/`Exit N`)
    /// instead of optimistic.
    pub fn child_exit_code_handle(&self) -> Arc<Mutex<Option<i32>>> {
        Arc::clone(&self.child_exit_code)
    }

    pub fn restore_raw_output_rx(&self, rx: RawOutputRx) {
        let mut slot = self.raw_output_rx.lock();
        if slot.is_none() {
            *slot = Some(rx);
        }
    }

    pub fn child_id(&self) -> Option<u32> {
        self.child.as_ref()?.lock().process_id()
    }

    pub fn is_alive(&self) -> bool {
        match &self.child {
            None => true, // Adopted engines have no child to track.
            Some(_) => self.try_wait().ok().flatten().is_none(),
        }
    }

    pub fn try_wait(&self) -> Result<Option<i32>, EngineError> {
        let Some(child) = &self.child else {
            return Ok(None);
        };
        match child.lock().try_wait() {
            Ok(Some(status)) => Ok(Some(status.exit_code().try_into().unwrap_or(-1))),
            Ok(None) => Ok(None),
            Err(e) => Err(EngineError::Pty(orkia_pty::PtyError::Wait(e.to_string()))),
        }
    }

    /// Returns a cheap, shareable closure that reports whether the child is
    /// still alive. Used by the attached renderer to auto-detach on exit
    /// without holding a reference to the engine itself. Adopted engines
    /// report alive forever (no child to track).
    pub fn liveness_probe(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
        match &self.child {
            None => Arc::new(|| true),
            Some(child) => {
                let child = Arc::clone(child);
                Arc::new(move || matches!(child.lock().try_wait(), Ok(None)))
            }
        }
    }

    /// A cheap, shareable closure that renders the current visible grid
    /// to ANSI bytes — the same output as [`Self::render_visible_snapshot`],
    /// but without holding the engine. The injection executor uses it to
    /// confirm a typed body actually landed in the agent's input box
    /// before submitting (Enter). Relies on the grid being live, which
    /// for agent engines means `EngineConfig::persistent_program`.
    pub fn grid_probe(&self) -> Arc<dyn Fn() -> Vec<u8> + Send + Sync> {
        let screen = Arc::clone(&self.screen);
        Arc::new(move || crate::render_snapshot::render_visible(&*screen.lock()).into_bytes())
    }
}

// ─── Process control ─────────────────────────────────────────────────────────

impl TerminalEngine {
    #[cfg(unix)]
    pub fn signal(&self, sig: i32) -> Result<(), EngineError> {
        let child = self.child.as_ref().ok_or_else(|| {
            EngineError::Pty(orkia_pty::PtyError::Backend(
                "adopted engine has no child to signal".into(),
            ))
        })?;
        // Hold the child lock across `try_wait` + `kill`. If the child
        // has already been reaped we report a backend error rather than
        // signal a recycled pid. The lock guarantees no
        // other owner of `self.child` can `try_wait` in the gap between
        // our liveness check and the `kill(2)` syscall — pid recycling
        // is therefore impossible while this critical section holds.
        let mut child = child.lock();
        if let Ok(Some(_)) = child.try_wait() {
            return Err(EngineError::Pty(orkia_pty::PtyError::Backend(
                "child already exited; cannot signal".into(),
            )));
        }
        let pid = child
            .process_id()
            .ok_or(EngineError::Pty(orkia_pty::PtyError::NoPid))?;
        // SAFETY: `kill(2)` with a pid sourced from our owned child
        // handle while the child-lock guarantees the pid is current.
        let ret = unsafe { libc::kill(pid as libc::pid_t, sig) };
        if ret == 0 {
            Ok(())
        } else {
            Err(EngineError::Pty(orkia_pty::PtyError::Io(
                std::io::Error::last_os_error(),
            )))
        }
    }
}

// ─── Internal constructor helpers ────────────────────────────────────────────

struct BuildEngineCtx {
    writer: SharedWriter,
    dims: SharedDims,
    screen: ScreenTerm,
    persistent_program: bool,
    buf_bytes: usize,
    on_osc133: Option<crate::blocks::Osc133Callback>,
    on_apc: Option<crate::blocks::ApcCallback>,
    init_cols: usize,
    init_rows: usize,
    min_publish: std::time::Duration,
    master: Option<SharedMaster>,
    adopted_master_fd: Option<Arc<OwnedFd>>,
    child: Option<SharedChild>,
    batch_prescan: bool,
}

fn spawn_cfg_from_engine(cfg: &EngineConfig) -> orkia_pty::SpawnConfig {
    orkia_pty::SpawnConfig {
        cols: cfg.init_cols,
        rows: cfg.init_rows,
        cmd: cfg.cmd.clone(),
        args: cfg.args.clone(),
        cwd: cfg.cwd.clone(),
        env: cfg.env.clone(),
    }
}

/// Build a `TerminalEngine` shell + a `ReaderCtx` for the reader thread.
/// The two fields that differ between `start` and `adopt_master`
/// (`child_exit_code`, `child_reaper`) are NOT set here — callers fill
/// them in before spawning the thread.
struct ParserSetup {
    blocks: SharedBlocks,
    sm: SharedState,
    parser: BlockParser,
    wake: Wake,
    wake_rx: crate::wake::WakeRx,
}

/// Build the block-parser, state machine, and extractor thread. Called
/// once per engine constructor; extracted so `build_engine_and_ctx` stays
/// under the 50-line function limit.
fn build_parser_setup(
    persistent_program: bool,
    on_osc133: Option<crate::blocks::Osc133Callback>,
    on_apc: Option<crate::blocks::ApcCallback>,
) -> ParserSetup {
    let blocks: SharedBlocks = Arc::new(Mutex::new(BlocksState::default()));
    let sm: SharedState = Arc::new(Mutex::new(if persistent_program {
        StateMachine::new_persistent()
    } else {
        StateMachine::new()
    }));
    let (wake, wake_rx) = wake_pair();
    let extract_dirty = Arc::new(AtomicBool::new(false));
    let mut parser = BlockParser::new(
        Arc::clone(&blocks),
        Arc::clone(&sm),
        Arc::clone(&extract_dirty),
    );
    if let Some(cb) = on_osc133 {
        parser.set_osc133_listener(cb);
    }
    if let Some(cb) = on_apc {
        parser.set_apc_listener(cb);
    }
    spawn_extractor(
        Arc::clone(&blocks),
        Arc::clone(&extract_dirty),
        wake.clone(),
    );
    ParserSetup {
        blocks,
        sm,
        parser,
        wake,
        wake_rx,
    }
}

fn build_engine_and_ctx(b: BuildEngineCtx) -> (TerminalEngine, ReaderCtx) {
    let ps = build_parser_setup(b.persistent_program, b.on_osc133, b.on_apc);
    let (raw_tx, raw_rx) = mpsc::sync_channel::<Vec<u8>>(RAW_OUTPUT_BOUND);
    let history = Arc::new(Mutex::new(HistoryRing::new()));
    let subscribers: Arc<Mutex<Vec<SubscriberTx>>> = Arc::new(Mutex::new(Vec::new()));

    let screen_publisher = ScreenPublisher::new(
        u16::try_from(b.init_cols).unwrap_or(u16::MAX),
        u16::try_from(b.init_rows).unwrap_or(u16::MAX),
        b.min_publish,
    );
    let screen_snapshot = Arc::clone(&screen_publisher.inner);
    let child_exited = Arc::new(AtomicBool::new(false));
    let child_exit_code: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
    let engine = TerminalEngine {
        writer: b.writer,
        blocks: Arc::clone(&ps.blocks),
        sm: Arc::clone(&ps.sm),
        screen: Arc::clone(&b.screen),
        master: b.master,
        adopted_master_fd: b.adopted_master_fd,
        dims: b.dims,
        wake: ps.wake.clone(),
        wake_rx: Some(ps.wake_rx),
        child: b.child,
        raw_output_rx: Mutex::new(Some(raw_rx)),
        history: Arc::clone(&history),
        subscribers: Arc::clone(&subscribers),
        screen_snapshot,
        child_exited: Arc::clone(&child_exited),
        child_exit_code: Arc::clone(&child_exit_code),
    };
    let reader_ctx = ReaderCtx {
        // reader/child_exit_code/child_reaper/child_exited overridden by caller
        reader: Box::new(std::io::empty()),
        buf_bytes: b.buf_bytes,
        raw_tx,
        history,
        subscribers,
        parser: ps.parser,
        sm: ps.sm,
        screen: b.screen,
        screen_publisher,
        wake: ps.wake,
        child_exited,
        child_exit_code: None, // overridden by caller
        child_reaper: None,    // overridden by caller
        batch_prescan: b.batch_prescan,
        prescanner: crate::prescan::PreScanner::new(),
    };
    (engine, reader_ctx)
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

//! PTY reader thread helpers: `HistoryRing` and `run_reader_loop`.
//!
//! Extracted from `engine.rs` to keep every file/impl/fn under the size
//! limits (REF-036). The reader loop is hot-path PTY code — byte handling,
//! grid/parser feeding, and channel sends are preserved verbatim from the
//! original two copies.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use alacritty_terminal::vte::ansi::Processor;
use parking_lot::Mutex;

use crate::blocks::BlockParser;
use crate::screen_view::ScreenPublisher;
use crate::state::{DisplayMode, SharedState};
use crate::wake::Wake;

use super::{RawOutputTx, SubscriberTx};

pub(super) const HISTORY_CAP: usize = 256 * 1024;

/// Append-only FIFO byte buffer capped at [`HISTORY_CAP`]. Older bytes
/// are dropped from the front when the cap is reached. Snapshotted as a
/// contiguous `Vec<u8>` for replay.
pub(super) struct HistoryRing {
    buf: std::collections::VecDeque<u8>,
}

impl HistoryRing {
    pub(super) fn new() -> Self {
        Self {
            buf: std::collections::VecDeque::with_capacity(HISTORY_CAP),
        }
    }

    pub(super) fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= HISTORY_CAP {
            // Single chunk bigger than the cap — keep only the tail.
            self.buf.clear();
            self.buf.extend(&bytes[bytes.len() - HISTORY_CAP..]);
            return;
        }
        let overflow = (self.buf.len() + bytes.len()).saturating_sub(HISTORY_CAP);
        for _ in 0..overflow {
            self.buf.pop_front();
        }
        self.buf.extend(bytes);
    }

    pub(super) fn snapshot(&self) -> Vec<u8> {
        let (a, b) = self.buf.as_slices();
        let mut out = Vec::with_capacity(a.len() + b.len());
        out.extend_from_slice(a);
        out.extend_from_slice(b);
        out
    }
}

/// Inputs to [`run_reader_loop`] — bundles all invariant captures so the
/// function stays under the 4-argument limit. Two genuine differences
/// between the `start` and `adopt_master` paths are carried here:
///
/// * `child_reaper`: present only for `start` (owned child). On EOF the
///   reader reaps the exit code into `child_exit_code` before flagging
///   `child_exited`.  `adopt_master` has no owned child so both fields
///   are `None` and the EOF arm is a no-op for the exit code.
///
/// * `batch_prescan`: `start` batches all `prescan` signals + the
///   display-mode read into one lock acquisition. The
///   `adopt_master` path preserves its original per-signal lock pattern
///   for a separate lock-per-observe + a final lock for display mode
///   (behaviour-preserving — the two constructors were already running
///   different code for this).
pub(super) struct ReaderCtx {
    pub reader: Box<dyn Read + Send>,
    pub buf_bytes: usize,
    pub raw_tx: RawOutputTx,
    pub history: Arc<Mutex<HistoryRing>>,
    pub subscribers: Arc<Mutex<Vec<SubscriberTx>>>,
    pub parser: BlockParser,
    pub sm: SharedState,
    pub screen: crate::ScreenTerm,
    pub screen_publisher: ScreenPublisher,
    pub wake: Wake,
    pub child_exited: Arc<AtomicBool>,
    /// Present only for `start` engines. `adopt_master` passes `None`.
    pub child_exit_code: Option<Arc<Mutex<Option<i32>>>>,
    /// Present only for `start` engines. `adopt_master` passes `None`.
    pub child_reaper: Option<super::SharedChild>,
    /// `true` for `start` (batches prescan + display-mode read into one
    /// lock acquisition per chunk). `false` for `adopt_master` (uses the
    /// original per-signal lock pattern).
    pub batch_prescan: bool,
    /// Stateful prescanner so a CSI private-mode sequence split across two
    /// reads isn't lost (BUG-104). Owned by the single reader thread.
    pub prescanner: crate::prescan::PreScanner,
}

/// Shared PTY reader loop, called from both `TerminalEngine::start` and
/// `TerminalEngine::adopt_master`. Runs in its own `std::thread::spawn`.
///
/// Behavior for each constructor is preserved exactly — see `ReaderCtx`
/// for the two parameterized differences.
pub(super) fn run_reader_loop(mut ctx: ReaderCtx) {
    let mut proc = Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
    let mut buf = vec![0u8; ctx.buf_bytes];
    loop {
        match ctx.reader.read(&mut buf) {
            Ok(0) | Err(_) => {
                on_eof(&mut ctx);
                break;
            }
            Ok(n) => {
                on_bytes(&buf[..n], &mut ctx, &mut proc);
            }
        }
    }
}

/// Handle a zero-byte read (EOF) or a read error — child exited.
fn on_eof(ctx: &mut ReaderCtx) {
    // Unified ordering across both constructors (BUG-102): write the reaped
    // exit code first (so any observer of `child_exited` reads a consistent
    // code), then set the flag, then notify the state machine. The
    // `adopt_master` path simply has no child to reap and skips the first step.
    if let (Some(code_slot), Some(reaper)) =
        (ctx.child_exit_code.as_ref(), ctx.child_reaper.as_ref())
    {
        *code_slot.lock() = bounded_child_exit_code(reaper);
    }
    ctx.child_exited.store(true, Ordering::SeqCst);
    ctx.sm.lock().notify_child_exited();

    // Final unconditional publish. A re-attach after child death must observe
    // the last visible state.
    let display = ctx.sm.lock().state().display;
    if display != DisplayMode::BlockView {
        let t = ctx.screen.lock();
        ctx.screen_publisher
            .publish(&*t, display, std::time::Instant::now());
    }
    ctx.wake.notify();
}

/// Handle a non-empty PTY read chunk.
fn on_bytes(
    bytes: &[u8],
    ctx: &mut ReaderCtx,
    proc: &mut Processor<alacritty_terminal::vte::ansi::StdSyncHandler>,
) {
    ctx.history.lock().push(bytes);
    send_raw_or_log(&ctx.raw_tx, bytes);
    fan_out_to_subscribers(&ctx.subscribers, bytes);
    ctx.parser.feed(bytes);

    // Stateful scan so a sequence split across reads isn't dropped (BUG-104).
    let signals = ctx.prescanner.scan(bytes);
    let display = if ctx.batch_prescan {
        // `start` path: batch all prescan signals + display-mode read
        // into one lock acquisition.
        let mut sm = ctx.sm.lock();
        for sig in signals {
            sm.observe(sig);
        }
        sm.state().display
    } else {
        // `adopt_master` path: per-signal lock (original behaviour).
        for sig in signals {
            ctx.sm.lock().observe(sig);
        }
        ctx.sm.lock().state().display
    };

    let screen_mode = display != DisplayMode::BlockView;
    if screen_mode {
        let mut t = ctx.screen.lock();
        proc.advance(&mut *t, bytes);
        ctx.screen_publisher
            .maybe_publish(&*t, display, std::time::Instant::now());
        ctx.wake.notify();
    }
}

/// Poll the child for its exit status for a short grace period after the
/// PTY hits EOF. `None` if it never reaps in time — the REPL's
/// authoritative `try_wait` collects the code later regardless.
fn bounded_child_exit_code(child: &super::SharedChild) -> Option<i32> {
    for _ in 0..8 {
        if let Ok(Some(status)) = child.lock().try_wait() {
            return Some(status.exit_code().try_into().unwrap_or(-1));
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    None
}

fn send_raw_or_log(tx: &RawOutputTx, bytes: &[u8]) {
    match tx.try_send(bytes.to_vec()) {
        Ok(()) => {}
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
        Err(std::sync::mpsc::TrySendError::Full(_)) => {
            tracing::warn!(
                "terminal-engine: raw_output channel full — dropping chunk ({} bytes)",
                bytes.len(),
            );
        }
    }
}

fn fan_out_to_subscribers(subs: &Arc<Mutex<Vec<SubscriberTx>>>, bytes: &[u8]) {
    let mut guard = subs.lock();
    guard.retain(|tx| match tx.try_send(bytes.to_vec()) {
        Ok(()) => true,
        Err(std::sync::mpsc::TrySendError::Full(_)) => {
            tracing::warn!(
                "terminal-engine: subscriber lagging — dropping chunk ({} bytes)",
                bytes.len(),
            );
            true
        }
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
    });
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Warp-style command blocks: a byte-stream parser that splits the hidden
//! shell's PTY output on OSC-133 markers, discarding prompt/echo noise and
//! keeping only each command's output + exit code.
//!
//! Threading model (lock-free snapshot, à la Zed/Warp): the PTY reader thread
//! is the *sole* owner and mutator of each block's alacritty grid (the
//! `BlockEngine`). It advances the grid per chunk and, coalesced to ≤120Hz,
//! extracts it into an immutable `Snapshot` published through `published`.
//! The render thread only ever clones that `Arc` — it never locks the term,
//! never walks the grid. Extraction is off the frame critical path entirely;
//! input echo no longer contends with output volume.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use parking_lot::Mutex;

use crate::ansi::{self, NoopProxy, Px, Snapshot};
use crate::state::SharedState;

/// Fixed per-block grid. No scrollback — output beyond `BLOCK_LINES`
/// scrolls off (like a terminal viewport), so extraction is O(visible).
pub const BLOCK_COLS: usize = 200;
pub const BLOCK_LINES: usize = 256;

/// Extractor cadence. The reader thread only advances the grid (lean — keeps
/// PTY drain + input echo tight); a *separate* extractor thread materialises
/// the Snapshot at ≤60Hz so extraction blocks neither the reader nor render.
const EXTRACT_MIN: Duration = Duration::from_millis(16); // ≤60Hz publishes
const EXTRACT_POLL: Duration = Duration::from_millis(4); // idle reaction time

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Status {
    Running,
    Done(i32),
}

/// The block's grid + vte state. Shared between exactly two threads — the
/// reader (advances it) and the extractor (snapshots it) — behind a *fair*
/// mutex so the hot reader cannot starve the periodic extractor. The render
/// thread never touches this (it reads the lock-free `published` Snapshot).
struct BlockEngine {
    term: Term<NoopProxy>,
    /// Per-block vte state — escape sequences never span commands.
    proc: Processor<StdSyncHandler>,
}

impl BlockEngine {
    fn new() -> Self {
        Self {
            term: ansi::new_block_term(BLOCK_COLS, BLOCK_LINES),
            proc: Processor::new(),
        }
    }
}

type Engine = Arc<FairMutex<BlockEngine>>;

pub struct Block {
    pub cmd: String,
    pub status: Status,
    pub started: Instant,
    pub ended: Option<Instant>,
    /// Latest extracted grid. Render clones this `Arc` lock-free.
    published: Arc<Mutex<Snapshot>>,
    /// Reader+extractor grid engine (render never locks this).
    engine: Engine,
}

impl Block {
    pub fn duration(&self) -> Option<Duration> {
        self.ended
            .map(|e| e.saturating_duration_since(self.started))
    }

    /// Wait-free on the render thread: one mutex-guarded `Arc` clone (the
    /// lock guards only a pointer, never real work — heavy extraction lives
    /// under the separate `engine` lock the renderer never takes).
    pub fn snapshot(&self) -> Snapshot {
        self.published.lock().clone()
    }
}

#[derive(Default)]
pub struct BlocksState {
    pub blocks: Vec<Block>,
}

impl BlocksState {
    /// Called by the input bar on Enter, before sending `cmd\r` to the shell.
    pub fn push_command(&mut self, cmd: String) {
        self.blocks.push(Block {
            cmd,
            status: Status::Running,
            started: Instant::now(),
            ended: None,
            published: Arc::new(Mutex::new(ansi::pack(Vec::new()))),
            engine: Arc::new(FairMutex::new(BlockEngine::new())),
        });
    }

    fn current(&self) -> Option<&Block> {
        self.blocks
            .iter()
            .rev()
            .find(|b| b.status == Status::Running)
    }

    fn current_mut(&mut self) -> Option<&mut Block> {
        self.blocks
            .iter_mut()
            .rev()
            .find(|b| b.status == Status::Running)
    }

    /// The running block, or (if none is running) the most recent one — so
    /// the extractor still publishes a just-finished command's final output.
    fn current_or_last(&self) -> Option<&Block> {
        self.current().or_else(|| self.blocks.last())
    }
}

/// Dedicated extractor: materialises the running/last block's grid into a
/// published Snapshot. Off the reader thread (keeps PTY drain + input echo
/// tight) and off the render thread (render only clones the `Arc`). Reacts
/// within `EXTRACT_POLL`, publishes at most every `EXTRACT_MIN` (≤60Hz).
pub fn spawn_extractor(
    blocks: SharedBlocks,
    extract_dirty: Arc<AtomicBool>,
    wake: crate::wake::Wake,
) {
    std::thread::spawn(move || {
        // Reused across iterations — zero per-extract allocation.
        let mut buf: Vec<Px> = Vec::new();
        loop {
            if !extract_dirty.swap(false, Ordering::AcqRel) {
                std::thread::sleep(EXTRACT_POLL);
                continue;
            }
            // Fixed-RATE loop: measure from the cycle start and sleep only
            // the remainder, so the publish period is max(EXTRACT_MIN,
            // bodywork) ≈ 16ms (60Hz) — not 16ms + bodywork.
            let cycle = Instant::now();
            // Brief state lock (≤60Hz) only to clone two Arcs.
            let target = {
                let st = blocks.lock();
                st.current_or_last()
                    .map(|b| (b.engine.clone(), b.published.clone()))
            };
            if let Some((engine, published)) = target {
                // into `buf`. The reader waits at most this (small) window
                // and FairMutex guarantees it the lock next.
                {
                    let e = engine.lock();
                    ansi::grid_cells(&e.term, BLOCK_COLS, BLOCK_LINES, &mut buf);
                }
                let v = ansi::cells_to_lines(&buf, BLOCK_COLS, BLOCK_LINES);
                *published.lock() = ansi::pack(v);
                let _ = cycle.elapsed();
                wake.notify();
            }
            let elapsed = cycle.elapsed();
            if elapsed < EXTRACT_MIN {
                std::thread::sleep(EXTRACT_MIN - elapsed);
            }
        }
    });
}

pub type SharedBlocks = Arc<Mutex<BlocksState>>;

/// OSC 133 (FinalTerm) shell-integration marker the [`BlockParser`]
/// reports to a registered callback. Lets higher layers
/// (`orkia-shell::protocol`) lift these markers into unified
/// [`OrkiaEvent`]s without re-parsing the byte stream.
#[derive(Debug, Clone, Copy)]
pub enum Osc133Marker {
    /// Marker `A` — prompt is about to be displayed.
    PromptStart,
    /// Marker `B` — prompt is displayed and waiting for input.
    PromptReady,
    /// Marker `C` — command is being executed; output begins.
    OutputStart,
    /// Marker `D[;N]` — output is done; exit code if reported.
    OutputFinished { exit_code: Option<i32> },
}

/// Callback type for OSC 133 markers. `Arc<dyn>` so the same
/// listener can be cloned across the engine's reader thread
/// without contention; the inner closure must be `Send + Sync`.
pub type Osc133Callback = Arc<dyn Fn(Osc133Marker) + Send + Sync>;

/// Callback for raw APC (`\x1b_...\x1b\\`) sequence payloads. The
/// payload bytes are the contents between `ESC _` and the
/// terminator (`ESC \` or `ST`). Used by the V2 Orkia protocol —
/// the protocol layer in `orkia-shell` checks the `Orkia;` prefix
/// and decodes the JSON tail.
pub type ApcCallback = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// Cheap handles to the running block, cloned out from under the state lock
/// so the heavy advance/extract runs with no lock held on `BlocksState`.
type Target = (Engine, Arc<Mutex<Snapshot>>);

/// Incremental OSC-133 parser, driven from the PTY reader thread.
pub struct BlockParser {
    state: SharedBlocks,
    sm: SharedState,
    /// Signals the extractor thread that the grid changed (not the render
    /// repaint flag — that is set by the extractor after it publishes).
    extract_dirty: Arc<AtomicBool>,
    capturing: bool,
    esc_pending: bool,
    in_osc: bool,
    osc_esc: bool,
    osc: Vec<u8>,
    /// Cached handles to the running block. Acquired once per command (on
    /// the OSC-133 `C` marker), reused for every chunk, released on `D` —
    /// so the hot reader never locks `BlocksState` per chunk (which would
    /// thrash against the render thread's whole-list lock).
    cur: Option<Target>,
    /// Optional listener fired for every recognised OSC 133 marker
    /// (A/B/C/D). Lets the protocol layer convert these to
    /// `OrkiaEvent`s without re-parsing the byte stream. `None` keeps
    /// the BlockParser hot-path overhead at zero.
    on_osc133: Option<Osc133Callback>,
    /// V2 APC protocol state: in the middle of an `ESC _ ... ESC \\`
    /// sequence. Same shape as the OSC state machine — bytes
    /// accumulate in `apc` until the terminator fires `finish_apc`.
    in_apc: bool,
    apc_esc: bool,
    apc: Vec<u8>,
    /// Listener for raw APC payloads. Fires once per complete APC
    /// sequence with the bytes between `ESC _` and the terminator.
    /// The protocol layer parses `Orkia;<json>` from these.
    on_apc: Option<ApcCallback>,
}

impl BlockParser {
    pub fn new(state: SharedBlocks, sm: SharedState, extract_dirty: Arc<AtomicBool>) -> Self {
        Self {
            state,
            sm,
            extract_dirty,
            capturing: false,
            esc_pending: false,
            in_osc: false,
            osc_esc: false,
            osc: Vec::new(),
            cur: None,
            on_osc133: None,
            in_apc: false,
            apc_esc: false,
            apc: Vec::new(),
            on_apc: None,
        }
    }

    /// Install an OSC 133 listener. Called from the protocol layer
    /// in `orkia-shell` to surface A/B/C/D markers as `OrkiaEvent`s.
    pub fn set_osc133_listener(&mut self, cb: Osc133Callback) {
        self.on_osc133 = Some(cb);
    }

    /// Install an APC sequence listener. Fires once per complete
    /// `ESC _ ... ESC \\` sequence with the payload bytes (the
    /// `Orkia;<json>` blob, prefix included).
    pub fn set_apc_listener(&mut self, cb: ApcCallback) {
        self.on_apc = Some(cb);
    }

    /// Cached running-block handles; locks `BlocksState` only on a cache
    /// miss (first chunk after a `C` marker), never per chunk thereafter.
    fn ensure_target(&mut self) -> Option<Target> {
        if self.cur.is_none() {
            let st = self.state.lock();
            // `current_or_last`, NOT `current`: a fast command can deliver
            // its output AND its OSC-133 `D` in a single PTY read. `D` is
            // processed mid-loop (block → Done) but the captured bytes are
            // flushed only after the loop — by then no block is `Running`.
            // Falling back to the just-finished `last` block (same rule the
            // extractor uses) lands that output instead of dropping it.
            self.cur = st
                .current_or_last()
                .map(|b| (b.engine.clone(), b.published.clone()));
        }
        self.cur.clone()
    }

    /// Advance the running block's grid with this chunk's output bytes.
    /// This is the `parse` phase (the pty reader times it). Lean — no
    /// extraction here; the extractor thread does that off-path.
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
        for &b in bytes {
            if self.in_osc {
                if self.osc_esc {
                    self.osc_esc = false;
                    if b == b'\\' {
                        self.finish_osc();
                        continue;
                    }
                }
                match b {
                    0x07 => self.finish_osc(),
                    0x1b => self.osc_esc = true,
                    _ => self.osc.push(b),
                }
                continue;
            }
            if self.in_apc {
                // APC is terminated by `ESC \` (ST) or by raw 0x9c
                // (single-byte ST). We don't accept BEL here — that
                // is an OSC-only convention.
                if self.apc_esc {
                    self.apc_esc = false;
                    if b == b'\\' {
                        self.finish_apc();
                        continue;
                    }
                    // ESC followed by something else inside APC is
                    // invalid; bail out of APC mode to avoid eating
                    // unbounded bytes from a malformed agent.
                    self.in_apc = false;
                    self.apc.clear();
                    continue;
                }
                match b {
                    0x9c => self.finish_apc(),
                    0x1b => self.apc_esc = true,
                    _ => self.apc.push(b),
                }
                continue;
            }
            if self.esc_pending {
                self.esc_pending = false;
                if b == b']' {
                    self.in_osc = true;
                    self.osc.clear();
                } else if b == b'_' {
                    self.in_apc = true;
                    self.apc.clear();
                } else if self.capturing {
                    // Keep non-OSC escapes (CSI/SGR colours, cursor moves).
                    out.push(0x1b);
                    out.push(b);
                }
                continue;
            }
            if b == 0x1b {
                self.esc_pending = true;
            } else if self.capturing {
                out.push(b);
            }
        }
        if !out.is_empty()
            && let Some((engine, _)) = self.ensure_target()
        {
            // Uncontended (reader-only) — never blocks on the renderer.
            let mut e = engine.lock();
            let e = &mut *e;
            e.proc.advance(&mut e.term, &out);
            self.extract_dirty.store(true, Ordering::Release);
        }
    }

    fn finish_osc(&mut self) {
        let payload = String::from_utf8_lossy(&self.osc).into_owned();
        self.in_osc = false;
        self.osc.clear();
        let Some(rest) = payload.strip_prefix("133;") else {
            return; // title / other OSC: drop
        };
        // Recognise the marker letter; A/B are pure side-events
        // (we don't alter block state for them, just notify the
        // protocol layer if a listener is installed). C/D are the
        // existing block-segmentation triggers.
        let marker = match rest.as_bytes().first() {
            Some(b'A') => Some(Osc133Marker::PromptStart),
            Some(b'B') => Some(Osc133Marker::PromptReady),
            Some(b'C') => {
                self.capturing = true;
                self.cur = None;
                self.sm.lock().set_capturing(true);
                Some(Osc133Marker::OutputStart)
            }
            Some(b'D') => {
                self.capturing = false;
                self.sm.lock().set_capturing(false);
                let parsed = rest
                    .split(';')
                    .nth(1)
                    .and_then(|s| s.trim().parse::<i32>().ok());
                let code_for_block = parsed.unwrap_or(0);
                let mut st = self.state.lock();
                if let Some(block) = st.current_mut() {
                    block.status = Status::Done(code_for_block);
                    block.ended = Some(Instant::now());
                }
                drop(st);
                self.cur = None;
                self.extract_dirty.store(true, Ordering::Release);
                Some(Osc133Marker::OutputFinished { exit_code: parsed })
            }
            _ => None,
        };
        // Fire the protocol callback last so it can't deadlock the
        // existing state-lock paths above.
        if let (Some(m), Some(cb)) = (marker, self.on_osc133.as_ref()) {
            cb(m);
        }
    }

    /// Called when an APC sequence (`ESC _ ... ESC \\`) terminates.
    /// Hands the raw payload bytes to the registered listener. The
    /// listener does the `Orkia;` prefix check + JSON decode in the
    /// protocol layer — `BlockParser` stays free of higher-level
    /// payload knowledge.
    fn finish_apc(&mut self) {
        self.in_apc = false;
        self.apc_esc = false;
        // Move the buffer out so we don't hold its allocation across
        // the callback (which may itself allocate / send / lock).
        let payload = std::mem::take(&mut self.apc);
        if let Some(cb) = self.on_apc.as_ref() {
            cb(&payload);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateMachine;

    /// Regression: a fast command whose output AND its OSC-133 `D` arrive
    /// in one PTY read must still capture the output (the
    /// "shell shows command + exit but no output" bug).
    #[test]
    fn fast_command_output_in_single_chunk_is_captured() {
        let blocks: SharedBlocks = Arc::new(Mutex::new(BlocksState::default()));
        blocks.lock().push_command("ll".to_string());
        let sm: SharedState = Arc::new(Mutex::new(StateMachine::new()));
        let dirty = Arc::new(AtomicBool::new(false));
        let mut parser = BlockParser::new(blocks.clone(), sm, dirty);

        // One chunk: OSC-133 C, the command output, OSC-133 D;0.
        let mut chunk = Vec::new();
        chunk.extend_from_slice(b"\x1b]133;C\x07");
        chunk.extend_from_slice(b"total 0\r\ndrwxr-xr-x  hello-output\r\n");
        chunk.extend_from_slice(b"\x1b]133;D;0\x07");
        parser.feed(&chunk);

        let st = blocks.lock();
        let block = st.blocks.last().expect("block exists");
        assert!(
            matches!(block.status, Status::Done(0)),
            "block should be Done(0), got {:?}",
            block.status
        );
        // Replicate the extractor; assert the output landed in the
        // just-finished block's grid (pre-fix this was empty).
        let mut buf: Vec<crate::ansi::Px> = Vec::new();
        {
            let e = block.engine.lock();
            crate::ansi::grid_cells(&e.term, BLOCK_COLS, BLOCK_LINES, &mut buf);
        }
        let lines = crate::ansi::cells_to_lines(&buf, BLOCK_COLS, BLOCK_LINES);
        let text: String = lines
            .iter()
            .flat_map(|l| l.iter().map(|s| s.text.clone()))
            .collect();
        assert!(
            text.contains("hello-output"),
            "fast command output was dropped; grid = {text:?}"
        );
    }
}

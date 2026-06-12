// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Minimal `alacritty_terminal::vte::ansi::Handler` implementation
//! that extracts the structural signals our prompt detector needs.
//!
//! This handler does NOT render anything. It owns no grid, no
//! scrollback, no SGR state. Every VTE method either updates
//! [`VteSignals`] or is a no-op (the trait provides default impls).
//! It lives inside the detector thread and is driven by a private
//! `vte::ansi::Processor` consuming bytes the engine fans out to us.
//!
//! The handler is intentionally byte-level cheap: a few field writes
//! per Handler method. The detector thread can keep up with claude's
//! bursts without backpressure.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use alacritty_terminal::vte::ansi::{
    CursorShape, CursorStyle, Handler, NamedPrivateMode, PrivateMode,
};

/// Cap on the per-line buffer fed to the classifier. A single TUI
/// frame line can be hundreds of cells wide; this is bounded so a
/// runaway line (no LF) does not blow memory.
const CURRENT_LINE_CAP: usize = 4096;

/// Number of completed lines retained for classification. The
/// classifier looks at the last ~10 lines for choice indicators.
const RECENT_LINES_CAP: usize = 16;

/// Raw structural signals extracted from the VTE stream. No
/// interpretation, no text matching. Owned by the detector thread —
/// no `Mutex` needed.
pub struct VteSignals {
    /// Wall-clock instant of the last `input` / `linefeed` / cursor /
    /// other activity from the agent.
    pub last_write_at: Instant,

    /// Number of `input(char)` calls since the last [`Self::reset_cycle`].
    /// Used by the detector to know "did the agent actually write
    /// anything in this cycle" — distinguishes a quiet idle from
    /// "we are at the start of a session, no output yet".
    pub write_count_since_reset: u64,

    /// True when the last VTE event was an explicit cursor move
    /// (`goto`, `goto_line`, `goto_col`). Reset to false on the next
    /// printable char. The "cursor positioned AFTER text" signal in
    /// the detector reads this flag.
    pub cursor_positioned_after_text: bool,

    /// Last-known cursor position from VTE cursor commands. 1-indexed
    /// per ANSI convention (the agent emits 1;1 for top-left).
    pub cursor_row: u32,
    pub cursor_col: u32,

    /// True while the agent has DECSET 1049 active (alt-screen).
    pub alt_screen: bool,

    /// The line currently being built — chars since the last LF/CR.
    /// Bounded by [`CURRENT_LINE_CAP`]. The classifier reads this
    /// for shell-prompt / y-n / radio-button patterns.
    pub current_line: String,

    /// Recently completed lines (LF terminated). Most recent at the
    /// back. Bounded by [`RECENT_LINES_CAP`].
    pub recent_lines: VecDeque<String>,
}

impl VteSignals {
    pub fn new() -> Self {
        Self {
            last_write_at: Instant::now(),
            write_count_since_reset: 0,
            cursor_positioned_after_text: false,
            cursor_row: 1,
            cursor_col: 1,
            alt_screen: false,
            current_line: String::new(),
            recent_lines: VecDeque::with_capacity(RECENT_LINES_CAP),
        }
    }

    pub fn idle_duration(&self) -> Duration {
        Instant::now().duration_since(self.last_write_at)
    }

    fn push_char(&mut self, c: char) {
        self.last_write_at = Instant::now();
        self.write_count_since_reset = self.write_count_since_reset.saturating_add(1);
        // Text after a cursor move ends the "cursor was the last
        // thing" state — the agent is still rendering.
        self.cursor_positioned_after_text = false;
        if self.current_line.len() < CURRENT_LINE_CAP {
            self.current_line.push(c);
        }
    }

    fn newline(&mut self) {
        self.last_write_at = Instant::now();
        let line = std::mem::take(&mut self.current_line);
        if self.recent_lines.len() >= RECENT_LINES_CAP {
            self.recent_lines.pop_front();
        }
        self.recent_lines.push_back(line);
    }

    fn cursor_move(&mut self, row: Option<u32>, col: Option<u32>) {
        self.last_write_at = Instant::now();
        if let Some(r) = row {
            self.cursor_row = r;
        }
        if let Some(c) = col {
            self.cursor_col = c;
        }
        self.cursor_positioned_after_text = true;
    }

    fn activity(&mut self) {
        self.last_write_at = Instant::now();
    }
}

impl Default for VteSignals {
    fn default() -> Self {
        Self::new()
    }
}

/// The Handler façade. Implements only the methods the detector cares
/// about; the trait's default `{}` bodies cover everything else.
///
/// `&'a mut VteSignals` lifetime keeps the interceptor cheap to build
/// per `Processor::advance` call — no allocation, no `Arc`.
pub struct VteInterceptor<'a> {
    pub signals: &'a mut VteSignals,
}

impl<'a> VteInterceptor<'a> {
    pub fn new(signals: &'a mut VteSignals) -> Self {
        Self { signals }
    }
}

impl Handler for VteInterceptor<'_> {
    fn input(&mut self, c: char) {
        self.signals.push_char(c);
    }

    fn linefeed(&mut self) {
        self.signals.newline();
    }

    fn newline(&mut self) {
        self.signals.newline();
    }

    fn carriage_return(&mut self) {
        // CR alone does not finish a logical line for our purposes;
        // many TUIs use \r to overwrite. Treat as activity only.
        self.signals.activity();
    }

    fn goto(&mut self, line: i32, col: usize) {
        let row = u32::try_from(line.max(0)).unwrap_or(0).saturating_add(1);
        let col = u32::try_from(col).unwrap_or(0).saturating_add(1);
        self.signals.cursor_move(Some(row), Some(col));
    }

    fn goto_line(&mut self, line: i32) {
        let row = u32::try_from(line.max(0)).unwrap_or(0).saturating_add(1);
        self.signals.cursor_move(Some(row), None);
    }

    fn goto_col(&mut self, col: usize) {
        let col = u32::try_from(col).unwrap_or(0).saturating_add(1);
        self.signals.cursor_move(None, Some(col));
    }

    fn move_up(&mut self, _: usize) {
        self.signals.activity();
    }

    fn move_down(&mut self, _: usize) {
        self.signals.activity();
    }

    fn move_forward(&mut self, _: usize) {
        self.signals.activity();
    }

    fn move_backward(&mut self, _: usize) {
        self.signals.activity();
    }

    fn move_down_and_cr(&mut self, _: usize) {
        self.signals.activity();
    }

    fn move_up_and_cr(&mut self, _: usize) {
        self.signals.activity();
    }

    fn set_private_mode(&mut self, mode: PrivateMode) {
        if let PrivateMode::Named(NamedPrivateMode::SwapScreenAndSetRestoreCursor) = mode {
            self.signals.alt_screen = true;
        }
        self.signals.activity();
    }

    fn unset_private_mode(&mut self, mode: PrivateMode) {
        if let PrivateMode::Named(NamedPrivateMode::SwapScreenAndSetRestoreCursor) = mode {
            self.signals.alt_screen = false;
        }
        self.signals.activity();
    }

    fn set_title(&mut self, _: Option<String>) {
        self.signals.activity();
    }

    fn set_cursor_style(&mut self, _: Option<CursorStyle>) {
        self.signals.activity();
    }

    fn set_cursor_shape(&mut self, _: CursorShape) {
        self.signals.activity();
    }

    fn bell(&mut self) {
        self.signals.activity();
    }
}

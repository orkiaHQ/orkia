// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Render a block's raw output bytes into colour-grouped text lines, by
//! replaying them through a throwaway alacritty grid (so SGR colours,
//! cursor moves, `\r` etc. resolve exactly like a real terminal would).

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::Config;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};

use crate::theme;

/// A run of text sharing a colour (and optional background, used for
/// inverse-video cells and the block cursor).
#[derive(Clone)]
pub struct Span {
    pub text: String,
    pub fg: u32,
    pub bg: Option<u32>,
}

/// One rendered line: a sequence of coloured spans.
pub type Line = Vec<Span>;

/// An immutable, shareable extracted grid. The reader thread publishes these;
/// the render thread clones the outer `Arc` (O(1), wait-free) and shares
/// per-row `Arc`s — no grid walk, no term lock on the frame path.
pub type Snapshot = std::sync::Arc<Vec<std::sync::Arc<Line>>>;

/// Wrap freshly-extracted lines into a shareable snapshot.
pub fn pack(lines: Vec<Line>) -> Snapshot {
    std::sync::Arc::new(lines.into_iter().map(std::sync::Arc::new).collect())
}

#[derive(Clone, Copy)]
struct Dims {
    cols: usize,
    lines: usize,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

#[derive(Clone)]
pub struct NoopProxy;
impl EventListener for NoopProxy {
    fn send_event(&self, _: Event) {}
}

/// A persistent per-block grid: fed incrementally by the reader thread
/// (never reparsed). No scrollback — a flooding command (`find /`) just
/// scrolls old rows off, bounding cost to the visible window.
pub fn new_block_term(cols: usize, lines: usize) -> Term<NoopProxy> {
    Term::new(
        Config {
            scrolling_history: 0,
            ..Config::default()
        },
        &Dims { cols, lines },
        NoopProxy,
    )
}

fn ansi_256(idx: u8) -> u32 {
    const BASE: [u32; 16] = [
        0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, 0x666666,
        0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff,
    ];
    match idx {
        0..=15 => BASE[idx as usize],
        16..=231 => {
            let i = idx - 16;
            let conv = |v: u8| -> u32 { if v == 0 { 0 } else { (v as u32) * 40 + 55 } };
            (conv(i / 36) << 16) | (conv((i % 36) / 6) << 8) | conv(i % 6)
        }
        _ => {
            let v = (idx - 232) as u32 * 10 + 8;
            (v << 16) | (v << 8) | v
        }
    }
}

fn named(nc: NamedColor) -> u32 {
    use NamedColor::*;
    match nc {
        Black => 0x000000,
        Red => 0xcd3131,
        Green => 0x0dbc79,
        Yellow => 0xe5e510,
        Blue => 0x2472c8,
        Magenta => 0xbc3fbc,
        Cyan => 0x11a8cd,
        White => 0xe5e5e5,
        BrightBlack => 0x666666,
        BrightRed => 0xf14c4c,
        BrightGreen => 0x23d18b,
        BrightYellow => 0xf5f543,
        BrightBlue => 0x3b8eea,
        BrightMagenta => 0xd670d6,
        BrightCyan => 0x29b8db,
        BrightWhite => 0xffffff,
        Background => theme::bg(),
        _ => theme::text_primary(),
    }
}

/// Darken a colour ~45% for DIM/faint text (claude's ghost suggestion).
fn dim(c: u32) -> u32 {
    let s = |v: u32| ((v as f32) * 0.55) as u32;
    (s((c >> 16) & 0xff) << 16) | (s((c >> 8) & 0xff) << 8) | s(c & 0xff)
}

fn fg_rgb(c: Color) -> u32 {
    match c {
        Color::Named(nc) => named(nc),
        Color::Spec(rgb) => ((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | rgb.b as u32,
        Color::Indexed(i) => ansi_256(i),
    }
}

/// One resolved grid cell: (char, fg, optional bg).
pub type Px = (char, u32, Option<u32>);

/// grid into a flat row-major buffer (reused across calls, no per-extract
/// allocation). Kept tight so the reader thread barely waits.
pub fn grid_cells<L: EventListener>(term: &Term<L>, cols: usize, lines: usize, buf: &mut Vec<Px>) {
    buf.clear();
    buf.resize(cols * lines, (' ', theme::text_primary(), None));
    let content = term.renderable_content();
    for ic in content.display_iter {
        let l = ic.point.line.0;
        let c = ic.point.column.0;
        if l < 0 || l as usize >= lines || c >= cols {
            continue;
        }
        let ch = ic.cell.c;
        let mut fg = fg_rgb(ic.cell.fg);
        let mut bg = None;
        // DIM/faint (claude's ghost suggestion, hints): darken the fg.
        if ic.cell.flags.contains(Flags::DIM) {
            fg = dim(fg);
        }
        // Inverse-video cells: swap fg/bg (claude selections, etc.).
        if ic.cell.flags.contains(Flags::INVERSE) {
            bg = Some(fg);
            fg = theme::bg();
        }
        buf[l as usize * cols + c] = (if ch == '\0' { ' ' } else { ch }, fg, bg);
    }
    // Block cursor (what iTerm draws on claude's input line).
    if content.cursor.shape != CursorShape::Hidden {
        let cl = content.cursor.point.line.0;
        let cc = content.cursor.point.column.0;
        if cl >= 0 && (cl as usize) < lines && cc < cols {
            let i = cl as usize * cols + cc;
            let ch = buf[i].0;
            buf[i] = (ch, theme::bg(), Some(theme::text_primary()));
        }
    }
}

/// spans and trim trailing blank lines. The allocation-heavy work, moved off
/// the contended engine lock.
pub fn cells_to_lines(buf: &[Px], cols: usize, lines: usize) -> Vec<Line> {
    // `chunks(0)` panics and `row[0]` would be out of bounds; this is a public
    // API so guard untrusted callers instead of panicking (BUG-103).
    if cols == 0 {
        return Vec::new();
    }
    let mut out: Vec<Line> = Vec::with_capacity(lines);
    for row in buf.chunks(cols).take(lines) {
        let mut line: Line = Vec::new();
        let mut run = String::new();
        let mut cur = (row[0].1, row[0].2);
        for &(ch, fg, bg) in row {
            if (fg, bg) != cur {
                if !run.is_empty() {
                    line.push(Span {
                        text: std::mem::take(&mut run),
                        fg: cur.0,
                        bg: cur.1,
                    });
                }
                cur = (fg, bg);
            }
            run.push(ch);
        }
        // Keep trailing run only if it has visible text or a background
        // (the cursor block at end-of-line must survive).
        if !run.trim_end().is_empty() || cur.1.is_some() {
            line.push(Span {
                text: run,
                fg: cur.0,
                bg: cur.1,
            });
        }
        if line.is_empty() {
            line.push(Span {
                text: String::new(),
                fg: theme::text_primary(),
                bg: None,
            });
        }
        out.push(line);
    }
    while out
        .last()
        .map(|l| l.iter().all(|s| s.text.trim().is_empty() && s.bg.is_none()))
        .unwrap_or(false)
    {
        out.pop();
    }
    out
}

/// Render any live alacritty `Term`'s visible grid into colour-grouped lines
/// (used for the live screen Term in Inline/AltScreen display modes, on the
pub fn term_to_lines<L: EventListener>(term: &Term<L>, cols: usize, lines: usize) -> Vec<Line> {
    // Public API: guard `cols == 0` before any grid math divides/indexes by it
    // (BUG-103).
    if cols == 0 {
        return Vec::new();
    }
    let mut buf = Vec::new();
    grid_cells(term, cols, lines, &mut buf);
    cells_to_lines(&buf, cols, lines)
}

/// Extract a packed Span snapshot AND the cursor position in a single grid
/// walk — used by the attached PTY widget so both grid and cursor are
/// internally consistent (no chance of the cursor moving between two locks).
pub fn term_to_snapshot_with_cursor<L: EventListener>(
    term: &Term<L>,
    cols: usize,
    lines: usize,
) -> (Snapshot, Option<crate::cursor::CursorInfo>) {
    let mut buf = Vec::new();
    grid_cells(term, cols, lines, &mut buf);
    let rows = cells_to_lines(&buf, cols, lines);
    let cursor = crate::cursor::extract_cursor(term, cols, lines);
    (pack(rows), cursor)
}

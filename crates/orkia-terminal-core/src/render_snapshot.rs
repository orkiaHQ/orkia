// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Render an alacritty `Term` grid back into ANSI bytes.
//!
//! Used by the attach pump as a replacement for raw byte replay
//! from the engine's history ring. Replay shows whatever the child
//! wrote — if the child redrew its UI N times (claude does this
//! across the trust-prompt → injected-body transition), the user
//! sees N stacked copies. Rendering from the grid instead always
//! produces exactly one screen worth of cells: the final visible
//! state of the terminal, regardless of how chaotic the byte
//! history was.
//!
//! Why not use alacritty's own renderer? It produces GPU
//! triangles, not ANSI. We emit a minimal SGR-prefixed UTF-8
//! stream — readable by any conforming terminal. Wide-char
//! spacers are skipped (the leading wide char already carries
//! both columns of visual width). Cursor position is restored at
//! the end so the live splice picks up where the grid left off.

use std::fmt::Write as _;

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

/// Render the currently-visible cells of `term` into a string of
/// ANSI-escaped UTF-8. Starts with full reset + clear, then walks
/// every row × column, emitting one SGR sequence per attribute
/// transition. The output is safe to dump directly to a host
/// terminal of the same (or larger) dimensions — the leading
/// clear + cursor-home guarantees we start from a known origin.
pub fn render_visible<T: EventListener>(term: &Term<T>) -> String {
    let grid = term.grid();
    let cols = grid.columns();
    let lines = grid.screen_lines();
    let mut out = String::with_capacity(lines * cols * 2);

    // Start from a known canvas: clear screen, home cursor, reset
    // SGR. Without this, any leftover SGR from previous host-
    // terminal output could colour the first cells we emit.
    out.push_str("\x1b[2J\x1b[H\x1b[0m");

    let mut active = SgrState::default();
    for row in 0..lines {
        let line = Line(row as i32);
        let mut col = 0usize;
        while col < cols {
            let cell = &grid[line][Column(col)];
            // The trailing half of a wide character; the leading
            // wide cell already carries the visible glyph. Skip
            // so we don't double-emit (and so cursor advances
            // exactly once per visual column).
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                col += 1;
                continue;
            }
            let desired = SgrState::from_cell(cell.fg, cell.bg, cell.flags);
            if desired != active {
                write_sgr_transition(&mut out, &active, &desired);
                active = desired;
            }
            // alacritty stores empty cells as ' '. Newlines and
            // tabs never appear in the grid (they were consumed
            // by the parser). Anything else goes through as-is.
            out.push(cell.c);
            col += 1;
        }
        if row + 1 < lines {
            // CR+LF: CR resets column to 0 on terminals that
            // don't honour LF alone in cooked-translation mode.
            // Combined with our line-by-line dump it always
            // lands us at column 0 of the next row.
            out.push_str("\r\n");
        }
    }

    // Reset attrs so post-replay output doesn't inherit the last
    // cell's colours, then move the cursor to where the child
    // believes it is so the live splice continues seamlessly.
    out.push_str("\x1b[0m");
    let cursor = grid.cursor.point;
    let cursor_row = cursor.line.0.max(0) as usize + 1;
    let cursor_col = cursor.column.0 + 1;
    let _ = write!(out, "\x1b[{cursor_row};{cursor_col}H");
    out
}

/// Cached SGR state so we emit only deltas between cells. Each
/// field captures what alacritty stored on the source cell;
/// transitions are translated into ANSI in
/// [`write_sgr_transition`].
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct SgrState {
    fg: Option<Color>,
    bg: Option<Color>,
    bold: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
    dim: bool,
    strikeout: bool,
}

impl SgrState {
    fn from_cell(fg: Color, bg: Color, flags: Flags) -> Self {
        Self {
            fg: Some(fg),
            bg: Some(bg),
            bold: flags.contains(Flags::BOLD),
            italic: flags.contains(Flags::ITALIC),
            underline: flags.intersects(Flags::ALL_UNDERLINES),
            inverse: flags.contains(Flags::INVERSE),
            dim: flags.contains(Flags::DIM),
            strikeout: flags.contains(Flags::STRIKEOUT),
        }
    }
}

/// Diff `prev` → `next` and emit the smallest reasonable ANSI
/// sequence to transition. For simplicity we emit a full reset +
/// re-apply on any change — bulletproof and cheap relative to
/// the actual cell data, which dwarfs the SGR bytes.
fn write_sgr_transition(out: &mut String, _prev: &SgrState, next: &SgrState) {
    out.push_str("\x1b[0m");
    if next.bold {
        out.push_str("\x1b[1m");
    }
    if next.dim {
        out.push_str("\x1b[2m");
    }
    if next.italic {
        out.push_str("\x1b[3m");
    }
    if next.underline {
        out.push_str("\x1b[4m");
    }
    if next.inverse {
        out.push_str("\x1b[7m");
    }
    if next.strikeout {
        out.push_str("\x1b[9m");
    }
    if let Some(fg) = next.fg {
        write_fg(out, fg);
    }
    if let Some(bg) = next.bg {
        write_bg(out, bg);
    }
}

fn write_fg(out: &mut String, c: Color) {
    match c {
        Color::Named(n) => match n {
            NamedColor::Foreground => {} // default fg — `\x1b[0m` already covers it
            NamedColor::Black => out.push_str("\x1b[30m"),
            NamedColor::Red => out.push_str("\x1b[31m"),
            NamedColor::Green => out.push_str("\x1b[32m"),
            NamedColor::Yellow => out.push_str("\x1b[33m"),
            NamedColor::Blue => out.push_str("\x1b[34m"),
            NamedColor::Magenta => out.push_str("\x1b[35m"),
            NamedColor::Cyan => out.push_str("\x1b[36m"),
            NamedColor::White => out.push_str("\x1b[37m"),
            NamedColor::BrightBlack => out.push_str("\x1b[90m"),
            NamedColor::BrightRed => out.push_str("\x1b[91m"),
            NamedColor::BrightGreen => out.push_str("\x1b[92m"),
            NamedColor::BrightYellow => out.push_str("\x1b[93m"),
            NamedColor::BrightMagenta => out.push_str("\x1b[95m"),
            NamedColor::BrightBlue => out.push_str("\x1b[94m"),
            NamedColor::BrightCyan => out.push_str("\x1b[96m"),
            NamedColor::BrightWhite => out.push_str("\x1b[97m"),
            // Dim variants and special cursor / bright-foreground
            // entries: emit nothing — the DIM/BOLD flags carry
            // the brightness intent, and these palette slots
            // would require querying the host's resolved RGB.
            _ => {}
        },
        Color::Indexed(idx) => {
            let _ = write!(out, "\x1b[38;5;{idx}m");
        }
        Color::Spec(Rgb { r, g, b }) => {
            let _ = write!(out, "\x1b[38;2;{r};{g};{b}m");
        }
    }
}

fn write_bg(out: &mut String, c: Color) {
    match c {
        Color::Named(n) => match n {
            NamedColor::Background => {} // default bg
            NamedColor::Black => out.push_str("\x1b[40m"),
            NamedColor::Red => out.push_str("\x1b[41m"),
            NamedColor::Green => out.push_str("\x1b[42m"),
            NamedColor::Yellow => out.push_str("\x1b[43m"),
            NamedColor::Blue => out.push_str("\x1b[44m"),
            NamedColor::Magenta => out.push_str("\x1b[45m"),
            NamedColor::Cyan => out.push_str("\x1b[46m"),
            NamedColor::White => out.push_str("\x1b[47m"),
            NamedColor::BrightBlack => out.push_str("\x1b[100m"),
            NamedColor::BrightRed => out.push_str("\x1b[101m"),
            NamedColor::BrightGreen => out.push_str("\x1b[102m"),
            NamedColor::BrightYellow => out.push_str("\x1b[103m"),
            NamedColor::BrightBlue => out.push_str("\x1b[104m"),
            NamedColor::BrightMagenta => out.push_str("\x1b[105m"),
            NamedColor::BrightCyan => out.push_str("\x1b[106m"),
            NamedColor::BrightWhite => out.push_str("\x1b[107m"),
            _ => {}
        },
        Color::Indexed(idx) => {
            let _ = write!(out, "\x1b[48;5;{idx}m");
        }
        Color::Spec(Rgb { r, g, b }) => {
            let _ = write!(out, "\x1b[48;2;{r};{g};{b}m");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::WindowSize;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::vte::ansi::Processor;

    #[derive(Clone, Default)]
    struct NoopListener;
    impl EventListener for NoopListener {
        fn send_event(&self, _: alacritty_terminal::event::Event) {}
    }

    fn make_term(cols: usize, rows: usize) -> Term<NoopListener> {
        let _ = WindowSize {
            num_lines: rows as u16,
            num_cols: cols as u16,
            cell_width: 0,
            cell_height: 0,
        };
        Term::new(Config::default(), &TermSize::new(cols, rows), NoopListener)
    }

    fn feed(term: &mut Term<NoopListener>, bytes: &[u8]) {
        let mut proc = Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
        proc.advance(term, bytes);
    }

    #[test]
    fn plain_text_renders_one_row() {
        let mut t = make_term(20, 3);
        feed(&mut t, b"hello");
        let rendered = render_visible(&t);
        assert!(
            rendered.starts_with("\x1b[2J\x1b[H\x1b[0m"),
            "missing prefix: {rendered:?}"
        );
        assert!(rendered.contains("hello"), "missing payload: {rendered:?}");
        // One CR+LF per row separator (rows - 1 = 2).
        assert_eq!(rendered.matches("\r\n").count(), 2);
    }

    #[test]
    fn no_history_replay_dupe() {
        // Simulate what claude does: draw a banner, then redraw
        // the SAME banner without clearing. With raw replay you
        // get two stacked banners. With grid render you get one —
        // because the grid only holds the final visible state.
        let mut t = make_term(20, 5);
        feed(&mut t, b"banner-A\r\n");
        feed(&mut t, b"\x1b[H"); // home cursor
        feed(&mut t, b"banner-B\r\n");
        let rendered = render_visible(&t);
        let count_a = rendered.matches("banner-A").count();
        let count_b = rendered.matches("banner-B").count();
        // banner-B overwrites banner-A's start; banner-A's tail
        // ("r-A") may survive past the shorter B if labels differ
        // in length. With equal-length labels: A is fully gone.
        assert_eq!(count_b, 1, "banner-B should appear exactly once");
        assert_eq!(count_a, 0, "banner-A should be overwritten");
    }

    #[test]
    fn cursor_position_restored() {
        let mut t = make_term(10, 3);
        feed(&mut t, b"abc");
        let rendered = render_visible(&t);
        // Cursor should be at row 1, column 4 (1-indexed, after "abc").
        assert!(
            rendered.ends_with("\x1b[1;4H"),
            "trailing CUP wrong: {rendered:?}"
        );
    }
}

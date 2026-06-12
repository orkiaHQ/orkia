// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Cursor extraction for the attached PTY widget. Reads the cursor position,
//! visibility, and shape from an alacritty `Term` so the renderer can paint
//! a cursor block on top of the snapshot grid.

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::vte::ansi::CursorShape as AlacCursorShape;

/// Cursor shape — mapped down from alacritty's richer enum to the three forms
/// the widget actually distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

/// A snapshot of the cursor at extraction time. `visible == false` means the
/// program has hidden the cursor (e.g. fullscreen pagers); the widget skips
/// painting in that case.
#[derive(Debug, Clone, Copy)]
pub struct CursorInfo {
    pub row: usize,
    pub col: usize,
    pub visible: bool,
    pub shape: CursorShape,
}

/// Extract the cursor from a live alacritty `Term`. Returns `None` when the
/// cursor would land outside the supplied dimensions (defensive — the parser
/// can transiently overshoot during resize). Caller must hold the screen lock.
pub fn extract_cursor<L: EventListener>(
    term: &Term<L>,
    cols: usize,
    rows: usize,
) -> Option<CursorInfo> {
    let content = term.renderable_content();
    let visible = content.cursor.shape != AlacCursorShape::Hidden;
    let row_signed = content.cursor.point.line.0;
    let col = content.cursor.point.column.0;
    if row_signed < 0 {
        return None;
    }
    let row = row_signed as usize;
    if row >= rows || col >= cols {
        return None;
    }
    let shape = match content.cursor.shape {
        AlacCursorShape::Underline => CursorShape::Underline,
        AlacCursorShape::Beam => CursorShape::Bar,
        // Block / HollowBlock / Hidden → Block (Hidden gates on `visible`).
        _ => CursorShape::Block,
    };
    Some(CursorInfo {
        row,
        col,
        visible,
        shape,
    })
}

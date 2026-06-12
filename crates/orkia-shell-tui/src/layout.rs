// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

use ratatui::layout::{Constraint, Direction, Layout, Rect};

const DEFAULT_SIDEBAR_WIDTH: u16 = 26;
const MIN_SIDEBAR_WIDTH: u16 = 20;
const MIN_TERMINAL_WIDTH: u16 = 60;

pub struct ShellLayout {
    pub sidebar_visible: bool,
    pub sidebar_width: u16,
}

impl Default for ShellLayout {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellLayout {
    pub fn new() -> Self {
        Self {
            sidebar_visible: true,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
        }
    }

    pub fn toggle_sidebar(&mut self) {
        self.sidebar_visible = !self.sidebar_visible;
    }

    /// Compute layout rects for a given terminal size.
    pub fn compute(&self, area: Rect) -> LayoutRects {
        let show_sidebar = self.sidebar_visible && area.width >= MIN_TERMINAL_WIDTH;

        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(area);

        let content_area = vertical[0];
        let input_area = vertical[1];
        let status_area = vertical[2];

        let (sidebar_area, main_area) = if show_sidebar {
            let w = self.sidebar_width.max(MIN_SIDEBAR_WIDTH);
            let horiz = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(w), Constraint::Min(1)])
                .split(content_area);
            (Some(horiz[0]), horiz[1])
        } else {
            (None, content_area)
        };

        LayoutRects {
            sidebar: sidebar_area,
            main: main_area,
            input: input_area,
            status: status_area,
        }
    }
}

pub struct LayoutRects {
    pub sidebar: Option<Rect>,
    pub main: Rect,
    pub input: Rect,
    pub status: Rect,
}

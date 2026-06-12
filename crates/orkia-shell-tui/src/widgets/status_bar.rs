// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

use crate::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub fn render_status_bar(f: &mut Frame<'_>, area: Rect, sidebar_visible: bool, theme: &Theme) {
    let toggle = if sidebar_visible { "hide" } else { "show" };
    // V1.1 (punchlist Item 2.7): advertise team-mode shortcuts
    // Ctrl-T (teams) and Ctrl-Shift-I (invite) alongside the
    // pre-existing global keys.
    let line = Line::from(vec![
        Span::styled("Ctrl-B ", Style::default().fg(theme.accent)),
        Span::styled(toggle, Style::default().fg(theme.dim)),
        Span::styled(" · ", Style::default().fg(theme.dim)),
        Span::styled("Ctrl-T", Style::default().fg(theme.accent)),
        Span::styled(" teams · ", Style::default().fg(theme.dim)),
        Span::styled("Ctrl-Shift-I", Style::default().fg(theme.accent)),
        Span::styled(
            " invite · @agent · /help · ",
            Style::default().fg(theme.dim),
        ),
        Span::styled("Alt-↑/↓", Style::default().fg(theme.accent)),
        Span::styled(" select · ", Style::default().fg(theme.dim)),
        Span::styled("Ctrl-O", Style::default().fg(theme.accent)),
        Span::styled(" fold · ", Style::default().fg(theme.dim)),
        Span::styled("Ctrl-D", Style::default().fg(theme.accent)),
        Span::styled(" exit", Style::default().fg(theme.dim)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

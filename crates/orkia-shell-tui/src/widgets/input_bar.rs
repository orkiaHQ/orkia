// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

use crate::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

/// Everything the input bar needs besides the frame, area and theme.
pub struct InputBarView<'a> {
    pub cwd: &'a str,
    pub rfc_scope: Option<&'a orkia_shell_types::RfcScopeSegment>,
    pub pending_approvals: usize,
    pub input: &'a str,
    pub cursor: usize,
}

pub fn render_input_bar(f: &mut Frame<'_>, area: Rect, view: &InputBarView<'_>, theme: &Theme) {
    // The input is an elevated, rounded "card" pinned above the status bar.
    let card = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .style(Style::default().bg(theme.bg_elevated));
    let inner = card.inner(area);
    f.render_widget(card, area);

    // Build the prompt prefix, tracking its column width so the input can
    // be horizontally scrolled to keep the cursor visible.
    let mut prefix_cols = 2 + view.cwd.chars().count(); // "⬡ " + cwd
    let mut spans = vec![
        Span::styled("⬡ ", Style::default().fg(theme.accent)),
        Span::styled(view.cwd.to_string(), Style::default().fg(theme.dim)),
    ];
    if let Some(seg) = view.rfc_scope {
        let rendered = seg.render();
        prefix_cols += 1 + rendered.chars().count();
        spans.push(Span::raw(" "));
        spans.push(Span::styled(rendered, Style::default().fg(theme.accent)));
    }
    if view.pending_approvals > 0 {
        let badge = format!(" [{} approval]", view.pending_approvals);
        prefix_cols += badge.chars().count();
        spans.push(Span::styled(badge, Style::default().fg(theme.yellow)));
    }
    prefix_cols += 3; // " ❯ "
    spans.push(Span::styled(
        " ❯ ",
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    ));

    // Horizontally window the buffer so the cursor stays on screen even
    // when the line is longer than the remaining width. The cursor cell
    // (a real char mid-line, or a trailing block at the end) occupies one
    // column and is kept inside the window.
    let chars: Vec<char> = view.input.chars().collect();
    let len = chars.len();
    let cur = view.cursor.min(len);
    let avail = (inner.width as usize).saturating_sub(prefix_cols).max(1);
    let start = if cur < avail { 0 } else { cur - avail + 1 };
    let end = (start + avail).min(len);
    for (k, ch) in chars[start..end].iter().enumerate() {
        if start + k == cur {
            spans.push(Span::styled(
                ch.to_string(),
                Style::default().bg(theme.accent).fg(theme.bg),
            ));
        } else {
            spans.push(Span::raw(ch.to_string()));
        }
    }
    if cur >= len {
        spans.push(Span::styled("█", Style::default().fg(theme.accent)));
    }

    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.bg_elevated)),
        inner,
    );
}

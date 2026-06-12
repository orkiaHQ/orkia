// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::source_refs::{SourceItem, SourceKind};
use crate::theme::Theme;
use crate::widgets::cockpit::CockpitModel;

pub fn selected_output_detail(model: &CockpitModel<'_>, theme: &Theme) -> Vec<Line<'static>> {
    let card = model
        .selected_card
        .and_then(|idx| model.cards.get(idx))
        .or_else(|| model.cards.iter().rev().find(|card| card.command.is_some()));
    let Some(card) = card else {
        return vec![Line::styled(
            "No output card selected.",
            Style::default().fg(theme.dim),
        )];
    };
    let sources = crate::source_refs::items_from_blocks(&card.blocks);
    if sources.is_empty() {
        return vec![
            Line::styled(
                "No navigable source refs in this card.",
                Style::default().fg(theme.dim),
            ),
            Line::styled(
                "Run operator ask --evidence to show citations.",
                Style::default().fg(theme.dim),
            ),
        ];
    }
    let selected = model
        .app
        .source_selected
        .min(sources.len().saturating_sub(1));
    let mut lines = vec![Line::styled(
        "source inspector",
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )];
    lines.push(Line::styled(
        "group  preview        reference",
        Style::default().fg(theme.dim),
    ));
    let mut last_kind = None;
    for (idx, item) in sources.iter().take(20).enumerate() {
        if last_kind != Some(item.kind) {
            lines.push(group_row(item.kind, theme));
            last_kind = Some(item.kind);
        }
        lines.push(source_row(idx == selected, item, theme));
    }
    lines.push(Line::styled(
        "j/k select source · o open · opened refs show source trail.",
        Style::default().fg(theme.dim),
    ));
    lines
}

fn group_row(kind: SourceKind, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled("  ", Style::default().fg(theme.dim)),
        Span::styled(
            kind.label().to_string(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn source_row(selected: bool, item: &SourceItem, theme: &Theme) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    let style = if selected {
        Style::default()
            .fg(theme.fg)
            .bg(theme.bg_selected)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.fg)
    };
    Line::from(vec![
        Span::styled(marker, style),
        Span::styled(
            format!("{:<7}", item.kind.label()),
            Style::default().fg(theme.dim),
        ),
        Span::styled(
            format!("{:<15}", item.preview),
            Style::default().fg(theme.dim),
        ),
        Span::styled(item.raw.clone(), style),
    ])
}

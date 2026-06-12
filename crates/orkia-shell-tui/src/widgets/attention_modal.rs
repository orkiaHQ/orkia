// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Attention queue modal.

use crossterm::event::{KeyCode, KeyEvent};
use orkia_shell_types::{AttentionAction, AttentionRow};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::theme::Theme;

#[derive(Debug, Clone, Default)]
pub struct AttentionModalState {
    pub visible: bool,
    pub rows: Vec<AttentionRow>,
    pub message: Option<String>,
    pub row_index: usize,
    pub action_index: usize,
}

impl AttentionModalState {
    pub fn open(&mut self, rows: Vec<AttentionRow>, message: Option<String>) {
        self.visible = !rows.is_empty();
        self.rows = rows;
        self.message = message;
        self.row_index = 0;
        self.action_index = 0;
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.rows.clear();
        self.message = None;
        self.row_index = 0;
        self.action_index = 0;
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AttentionModalAction {
    Enqueue(String),
    Close,
}

pub fn render_attention_modal(
    f: &mut Frame<'_>,
    area: Rect,
    state: &AttentionModalState,
    theme: &Theme,
) {
    if !state.visible {
        return;
    }
    let Some(row) = state.rows.get(state.row_index) else {
        return;
    };
    let title = format!(
        " Attention · @{} · {} · {} ",
        row.agent,
        row.kind.as_str(),
        row.age
    );
    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme.yellow)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL);
    let mut lines = Vec::new();
    if let Some(message) = &state.message {
        lines.push(Line::styled(
            message.clone(),
            Style::default().fg(theme.dim),
        ));
        lines.push(Line::raw(""));
    }
    lines.push(Line::from(vec![
        Span::styled(
            row.id.to_string(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            row.severity.as_str().to_string(),
            Style::default().fg(theme.status_color(row.severity.as_str())),
        ),
    ]));
    lines.push(Line::raw(""));
    for line in row.summary.lines().take(5) {
        lines.push(Line::raw(line.to_string()));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("Actions", Style::default().fg(theme.dim)));
    if row.actions.is_empty() {
        lines.push(Line::styled("  none", Style::default().fg(theme.dim)));
    } else {
        for (idx, action) in row.actions.iter().enumerate() {
            let selected = idx == state.action_index;
            let marker = if selected { "❯ " } else { "  " };
            let style = if selected {
                Style::default()
                    .fg(theme.bg)
                    .bg(theme.yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.fg)
            };
            lines.push(Line::styled(
                format!("{marker}{}", action_label(action)),
                style,
            ));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "↑/↓ action · ←/→ entry · Enter apply · Esc close",
        Style::default().fg(theme.dim),
    ));

    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg)),
        area,
    );
}

pub fn handle_key(state: &mut AttentionModalState, key: KeyEvent) -> Option<AttentionModalAction> {
    if !state.visible {
        return None;
    }
    match key.code {
        KeyCode::Esc => {
            state.close();
            Some(AttentionModalAction::Close)
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.action_index = state.action_index.saturating_sub(1);
            Some(AttentionModalAction::Close)
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(row) = state.rows.get(state.row_index) {
                state.action_index =
                    (state.action_index + 1).min(row.actions.len().saturating_sub(1));
            }
            Some(AttentionModalAction::Close)
        }
        KeyCode::Left | KeyCode::Char('h') => {
            state.row_index = state.row_index.saturating_sub(1);
            state.action_index = 0;
            Some(AttentionModalAction::Close)
        }
        KeyCode::Right | KeyCode::Char('l') => {
            if !state.rows.is_empty() {
                state.row_index = (state.row_index + 1).min(state.rows.len() - 1);
            }
            state.action_index = 0;
            Some(AttentionModalAction::Close)
        }
        KeyCode::Enter => commit(state),
        _ => Some(AttentionModalAction::Close),
    }
}

fn commit(state: &mut AttentionModalState) -> Option<AttentionModalAction> {
    let row = state.rows.get(state.row_index)?;
    let action = row.actions.get(state.action_index)?;
    let cmd = format!("attention resolve {} {}", row.id, action.as_str());
    state.close();
    Some(AttentionModalAction::Enqueue(cmd))
}

fn action_label(action: &AttentionAction) -> String {
    match action {
        AttentionAction::Hold => "hold — defer Orkia-driven actions".into(),
        AttentionAction::AbortAgent(agent) => format!("abort-{agent} — stop requester job"),
        AttentionAction::ProceedAnyway => "proceed-anyway — override conflict".into(),
        AttentionAction::Allow => "allow — approve request".into(),
        AttentionAction::Deny => "deny — reject request".into(),
        AttentionAction::Inspect => "inspect — show details".into(),
        AttentionAction::Pull => "pull — open queued item".into(),
        AttentionAction::Resolve => "resolve".into(),
    }
}

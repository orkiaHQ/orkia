// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//!
//! Opened from `TeamPane` via Enter. Renders team metadata + member
//! list. Keybindings `a` / `r` / `c` enqueue the matching
//! `$members` builtin in templated form — the user fills in the
//! account/agent id at the prompt.

use crossterm::event::{KeyCode, KeyEvent};
use orkia_shell_types::{TeamSnapshot, TeamSummary};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use uuid::Uuid;

use super::team_color::hex_to_color;
use crate::theme::Theme;

#[derive(Debug, Clone, Default)]
pub struct TeamDetailState {
    pub team_id: Option<Uuid>,
}

impl TeamDetailState {
    pub fn open(&mut self, team_id: Uuid) {
        self.team_id = Some(team_id);
    }
    pub fn close(&mut self) {
        self.team_id = None;
    }
    pub fn is_visible(&self) -> bool {
        self.team_id.is_some()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TeamDetailAction {
    Enqueue(String),
    Close,
}

pub fn render_team_detail(
    f: &mut Frame<'_>,
    area: Rect,
    state: &TeamDetailState,
    snapshot: &TeamSnapshot,
    theme: &Theme,
) {
    let Some(team_id) = state.team_id else {
        return;
    };
    let Some(team) = snapshot.teams.iter().find(|t| t.id == team_id) else {
        return;
    };

    let title_color = team
        .color
        .as_deref()
        .and_then(hex_to_color)
        .unwrap_or(theme.accent);
    let block = Block::default()
        .title(Span::styled(
            format!(" Team: {} ", team.identifier),
            Style::default()
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL);

    let mut lines = render_team_metadata(team, title_color, theme);
    render_team_members(snapshot, team.id, &mut lines, theme);
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "[a] add  [r] remove  [c] change-role  [esc] close",
        Style::default().fg(theme.dim),
    ));

    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(Clear, area);
    f.render_widget(para, area);
}

fn render_team_metadata(
    team: &TeamSummary,
    title_color: ratatui::style::Color,
    theme: &Theme,
) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled("Name:        ", Style::default().fg(theme.dim)),
            Span::raw(team.name.clone()),
        ]),
        Line::from(vec![
            Span::styled("Identifier:  ", Style::default().fg(theme.dim)),
            Span::raw(team.identifier.clone()),
        ]),
        Line::from(vec![
            Span::styled("Description: ", Style::default().fg(theme.dim)),
            Span::raw(team.description.clone().unwrap_or_else(|| "(none)".into())),
        ]),
        Line::from(vec![
            Span::styled("Color:       ", Style::default().fg(theme.dim)),
            Span::styled(
                team.color.clone().unwrap_or_else(|| "(none)".into()),
                Style::default().fg(title_color),
            ),
        ]),
        Line::from(vec![
            Span::styled("Owner:       ", Style::default().fg(theme.dim)),
            Span::raw(team.owner_account_id.to_string()),
        ]),
        Line::raw(""),
    ]
}

fn render_team_members(
    snapshot: &TeamSnapshot,
    team_id: Uuid,
    lines: &mut Vec<Line<'static>>,
    theme: &Theme,
) {
    let members: Vec<_> = snapshot
        .team_members
        .iter()
        .filter(|m| m.team_id == team_id)
        .collect();
    lines.push(Line::styled(
        format!("Members ({}):", members.len()),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    for m in &members {
        let who = m
            .account_id
            .map(|a| a.to_string())
            .or_else(|| m.agent_name.clone())
            .unwrap_or_else(|| "?".into());
        let prefix = if m.agent_name.is_some() { "─" } else { " " };
        lines.push(Line::from(vec![
            Span::raw(format!(" {prefix} ")),
            Span::raw(who),
            Span::raw("  "),
            Span::styled(m.role.clone(), Style::default().fg(theme.dim)),
        ]));
    }
}

pub fn handle_key(state: &mut TeamDetailState, key: KeyEvent) -> Option<TeamDetailAction> {
    let team_id = state.team_id?;
    match key.code {
        KeyCode::Esc => {
            state.close();
            Some(TeamDetailAction::Close)
        }
        KeyCode::Char('a') => {
            state.close();
            Some(TeamDetailAction::Enqueue(format!(
                "members add  --team {team_id} --role member"
            )))
        }
        KeyCode::Char('r') => {
            state.close();
            Some(TeamDetailAction::Enqueue(format!(
                "members rm  --team {team_id}"
            )))
        }
        KeyCode::Char('c') => {
            state.close();
            Some(TeamDetailAction::Enqueue(format!(
                "members role   --team {team_id}"
            )))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn esc_closes() {
        let mut s = TeamDetailState {
            team_id: Some(Uuid::new_v4()),
        };
        let act = handle_key(&mut s, key(KeyCode::Esc)).unwrap();
        assert_eq!(act, TeamDetailAction::Close);
        assert!(!s.is_visible());
    }

    #[test]
    fn a_enqueues_members_add_template() {
        let tid = Uuid::new_v4();
        let mut s = TeamDetailState { team_id: Some(tid) };
        let act = handle_key(&mut s, key(KeyCode::Char('a'))).unwrap();
        match act {
            TeamDetailAction::Enqueue(cmd) => {
                assert!(cmd.starts_with("members add"));
                assert!(cmd.contains(&format!("--team {tid}")));
                assert!(cmd.contains("--role member"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn r_enqueues_remove_template() {
        let tid = Uuid::new_v4();
        let mut s = TeamDetailState { team_id: Some(tid) };
        let act = handle_key(&mut s, key(KeyCode::Char('r'))).unwrap();
        match act {
            TeamDetailAction::Enqueue(cmd) => {
                assert!(cmd.starts_with("members rm"));
                assert!(cmd.contains(&format!("--team {tid}")));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

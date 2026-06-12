// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//!
//! Modal-ish popup that lists every team in the current workspace
//! with a presence dot and the caller's role. Stateless render
//! function paired with a [`TeamPaneState`] that the TUI owns and
//! mutates from key events. Actions returned by `handle_key` are
//! resolved server-side via the shell builtins —
//! [`TeamAction::Enqueue`] is the only kind of side effect this
//! widget produces.

use crossterm::event::{KeyCode, KeyEvent};
use orkia_shell_types::TeamSnapshot;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState};
use uuid::Uuid;

use super::team_color::hex_to_color;
use crate::theme::Theme;

/// Per-row layout used by both the renderer and the navigation logic
/// in `handle_key`. Computed once per render call from the snapshot
/// — keeps the visible team set and the selection-index arithmetic
/// consistent.
#[derive(Debug, Clone)]
struct PaneRow {
    team_id: Uuid,
    label: String,
    color_hex: Option<String>,
    /// Kept for presence-indicator work (dimming non-member rows).
    /// Not consumed today.
    #[allow(dead_code)]
    am_member: bool,
}

#[derive(Debug, Clone, Default)]
pub struct TeamPaneState {
    pub visible: bool,
    pub selected: usize,
}

impl TeamPaneState {
    pub fn toggle(&mut self) {
        self.visible = !self.visible;
        if self.visible {
            self.selected = 0;
        }
    }
    pub fn close(&mut self) {
        self.visible = false;
    }
    pub fn is_visible(&self) -> bool {
        self.visible
    }
}

/// Action the TUI executes after the user interacts with the pane.
/// The TUI typically maps these onto a shell-builtin invocation.
#[derive(Debug, Clone, PartialEq)]
pub enum TeamPaneAction {
    OpenDetail(Uuid),
    Enqueue(String),
    Close,
}

pub fn render_team_pane(
    f: &mut Frame<'_>,
    area: Rect,
    state: &TeamPaneState,
    snapshot: &TeamSnapshot,
    current_team: Option<Uuid>,
    theme: &Theme,
) {
    if !state.visible {
        return;
    }
    let rows = compute_rows(snapshot, current_team);
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            let style = row
                .color_hex
                .as_deref()
                .and_then(hex_to_color)
                .map(|c| Style::default().fg(c))
                .unwrap_or_default();
            ListItem::new(row.label.clone()).style(style)
        })
        .collect();

    let title = if rows.is_empty() {
        "Teams (none — press n to create)"
    } else {
        "Teams"
    };
    let list = List::new(items)
        .block(Block::default().title(title).borders(Borders::ALL))
        .highlight_style(
            Style::default()
                .add_modifier(Modifier::REVERSED)
                .fg(theme.accent),
        );

    let mut list_state = ListState::default();
    list_state.select(if rows.is_empty() {
        None
    } else {
        Some(state.selected.min(rows.len() - 1))
    });

    f.render_widget(Clear, area);
    f.render_stateful_widget(list, area, &mut list_state);
}

pub fn handle_key(
    state: &mut TeamPaneState,
    snapshot: &TeamSnapshot,
    current_team: Option<Uuid>,
    key: KeyEvent,
) -> Option<TeamPaneAction> {
    if !state.visible {
        return None;
    }
    let rows = compute_rows(snapshot, current_team);
    let count = rows.len();
    match key.code {
        KeyCode::Esc => {
            state.close();
            Some(TeamPaneAction::Close)
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if state.selected > 0 {
                state.selected -= 1;
            }
            None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if state.selected + 1 < count {
                state.selected += 1;
            }
            None
        }
        KeyCode::Enter => rows.get(state.selected).map(|row| {
            state.close();
            TeamPaneAction::OpenDetail(row.team_id)
        }),
        KeyCode::Char('n') => {
            state.close();
            Some(TeamPaneAction::Enqueue("team create ".into()))
        }
        KeyCode::Char('d') => rows.get(state.selected).map(|row| {
            state.close();
            TeamPaneAction::Enqueue(format!("team rm {} --yes", row.team_id))
        }),
        KeyCode::Char('r') => {
            state.close();
            Some(TeamPaneAction::Enqueue("team refresh".into()))
        }
        _ => None,
    }
}

fn compute_rows(snapshot: &TeamSnapshot, current_team: Option<Uuid>) -> Vec<PaneRow> {
    snapshot
        .teams
        .iter()
        .map(|team| {
            let am_member = snapshot.team_scope.contains(&team.id);
            let is_current = current_team == Some(team.id);
            let prefix = if is_current {
                "\u{25b6}" // ▶
            } else if am_member {
                "\u{25cf}" // ●
            } else {
                "\u{25cb}" // ○
            };
            let members = snapshot
                .team_members
                .iter()
                .filter(|m| m.team_id == team.id)
                .count();
            let label = format!(
                "{} {:<16} {:<24} {} member(s)",
                prefix, team.identifier, team.name, members
            );
            PaneRow {
                team_id: team.id,
                label,
                color_hex: team.color.clone(),
                am_member,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use orkia_shell_types::TeamSummary;

    fn snapshot_with(team_ids: Vec<(&str, &str)>) -> TeamSnapshot {
        let mut snap = TeamSnapshot::default();
        for (ident, name) in team_ids {
            snap.teams.push(TeamSummary {
                id: Uuid::new_v4(),
                identifier: ident.into(),
                name: name.into(),
                description: None,
                color: Some("#112233".into()),
                owner_account_id: Uuid::new_v4(),
            });
        }
        snap
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn toggle_flips_visibility() {
        let mut s = TeamPaneState::default();
        assert!(!s.visible);
        s.toggle();
        assert!(s.visible);
        s.toggle();
        assert!(!s.visible);
    }

    #[test]
    fn down_arrow_advances_selection_bounded() {
        let snap = snapshot_with(vec![("a", "A"), ("b", "B"), ("c", "C")]);
        let mut s = TeamPaneState {
            visible: true,
            selected: 0,
        };
        handle_key(&mut s, &snap, None, key(KeyCode::Down));
        assert_eq!(s.selected, 1);
        handle_key(&mut s, &snap, None, key(KeyCode::Down));
        assert_eq!(s.selected, 2);
        handle_key(&mut s, &snap, None, key(KeyCode::Down));
        assert_eq!(s.selected, 2); // clamped
    }

    #[test]
    fn enter_returns_open_detail_for_selected() {
        let snap = snapshot_with(vec![("a", "A"), ("b", "B")]);
        let mut s = TeamPaneState {
            visible: true,
            selected: 1,
        };
        let action = handle_key(&mut s, &snap, None, key(KeyCode::Enter)).unwrap();
        match action {
            TeamPaneAction::OpenDetail(id) => assert_eq!(id, snap.teams[1].id),
            other => panic!("unexpected action {other:?}"),
        }
        assert!(!s.visible);
    }

    #[test]
    fn esc_closes_pane() {
        let snap = snapshot_with(vec![("a", "A")]);
        let mut s = TeamPaneState {
            visible: true,
            selected: 0,
        };
        let act = handle_key(&mut s, &snap, None, key(KeyCode::Esc)).unwrap();
        assert_eq!(act, TeamPaneAction::Close);
        assert!(!s.visible);
    }

    #[test]
    fn d_enqueues_rm_for_selected_team() {
        let snap = snapshot_with(vec![("eng", "Eng")]);
        let mut s = TeamPaneState {
            visible: true,
            selected: 0,
        };
        let act = handle_key(&mut s, &snap, None, key(KeyCode::Char('d'))).unwrap();
        match act {
            TeamPaneAction::Enqueue(cmd) => {
                assert!(cmd.starts_with("team rm "));
                assert!(cmd.ends_with(" --yes"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn ignores_keys_when_hidden() {
        let snap = snapshot_with(vec![("a", "A")]);
        let mut s = TeamPaneState::default();
        assert!(handle_key(&mut s, &snap, None, key(KeyCode::Enter)).is_none());
    }

    #[test]
    fn current_team_shown_with_arrow_prefix() {
        let snap = snapshot_with(vec![("a", "A"), ("b", "B")]);
        let current = snap.teams[1].id;
        let rows = compute_rows(&snap, Some(current));
        assert!(rows[1].label.starts_with('\u{25b6}'));
        assert!(rows[0].label.starts_with('\u{25cb}'));
        let _ = rows[1].am_member; // field exists for future presence work
    }
}

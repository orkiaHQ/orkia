// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//!
//! Two-field modal: target workspace UUID + access level. Tab cycles
//! between the input and the access toggle. Enter dispatches the
//! matching `$share` builtin; Esc closes.
//!
//! **Not yet wired.** The render and key-routing paths are in place
//! (`render_share_dialog`, `handle_key`), but no runtime keybinding
//! calls [`ShareDialogState::open_for_project`] /
//! [`ShareDialogState::open_for_issue`] — so `is_visible()` is always
//! `false` and the modal never appears. Opening it is pending the
//! widget is exercised by unit tests only; do not assume it is
//! reachable from the UI until an `open_for_*` call is wired in.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use uuid::Uuid;

use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShareSubject {
    Project(Uuid),
    Issue(Uuid),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShareAccess {
    Read,
    Write,
    Admin,
}

impl ShareAccess {
    pub fn next(self) -> Self {
        match self {
            Self::Read => Self::Write,
            Self::Write => Self::Admin,
            Self::Admin => Self::Read,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Admin => "admin",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
enum Focus {
    #[default]
    Target,
    Access,
}

#[derive(Debug, Clone, Default)]
pub struct ShareDialogState {
    pub visible: bool,
    pub subject: Option<ShareSubject>,
    pub target_input: String,
    pub access: Option<ShareAccess>,
    focus: Focus,
}

impl ShareDialogState {
    pub fn open_for_project(&mut self, id: Uuid) {
        self.visible = true;
        self.subject = Some(ShareSubject::Project(id));
        self.target_input.clear();
        self.access = Some(ShareAccess::Read);
        self.focus = Focus::Target;
    }
    pub fn open_for_issue(&mut self, id: Uuid) {
        self.visible = true;
        self.subject = Some(ShareSubject::Issue(id));
        self.target_input.clear();
        self.access = Some(ShareAccess::Read);
        self.focus = Focus::Target;
    }
    pub fn close(&mut self) {
        self.visible = false;
        self.subject = None;
        self.target_input.clear();
        self.access = None;
        self.focus = Focus::Target;
    }
    pub fn is_visible(&self) -> bool {
        self.visible
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ShareDialogAction {
    Enqueue(String),
    Close,
    InvalidTarget,
}

pub fn render_share_dialog(f: &mut Frame<'_>, area: Rect, state: &ShareDialogState, theme: &Theme) {
    if !state.visible {
        return;
    }
    let title = match state.subject {
        Some(ShareSubject::Project(_)) => " Share Project ",
        Some(ShareSubject::Issue(_)) => " Share Issue ",
        None => " Share ",
    };
    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL);

    let subject_label = match state.subject {
        Some(ShareSubject::Project(id)) => format!("Project: {id}"),
        Some(ShareSubject::Issue(id)) => format!("Issue:   {id}"),
        None => "Subject: (none)".into(),
    };

    let target_style = if state.focus == Focus::Target {
        Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(theme.accent)
    } else {
        Style::default()
    };
    let access_label = state.access.map(ShareAccess::label).unwrap_or("read");
    let access_style = if state.focus == Focus::Access {
        Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(theme.accent)
    } else {
        Style::default()
    };

    let lines = vec![
        Line::raw(subject_label),
        Line::raw(""),
        Line::from(vec![
            Span::styled("Target workspace: ", Style::default().fg(theme.dim)),
            Span::styled(state.target_input.clone(), target_style),
            Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
        ]),
        Line::from(vec![
            Span::styled("Access:           ", Style::default().fg(theme.dim)),
            Span::styled(access_label, access_style),
        ]),
        Line::raw(""),
        Line::styled(
            "[Tab] focus  [\u{2190}/\u{2192}] cycle access  [Enter] share  [Esc] cancel",
            Style::default().fg(theme.dim),
        ),
    ];

    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

pub fn handle_key(state: &mut ShareDialogState, key: KeyEvent) -> Option<ShareDialogAction> {
    if !state.visible {
        return None;
    }
    match key.code {
        KeyCode::Esc => {
            state.close();
            Some(ShareDialogAction::Close)
        }
        KeyCode::Tab => {
            state.focus = match state.focus {
                Focus::Target => Focus::Access,
                Focus::Access => Focus::Target,
            };
            None
        }
        KeyCode::Left | KeyCode::Right => {
            if state.focus == Focus::Access {
                state.access = Some(state.access.unwrap_or(ShareAccess::Read).next());
            }
            None
        }
        KeyCode::Enter => Some(commit(state)),
        KeyCode::Backspace if state.focus == Focus::Target => {
            state.target_input.pop();
            None
        }
        KeyCode::Char(c) if state.focus == Focus::Target => {
            state.target_input.push(c);
            None
        }
        _ => None,
    }
}

fn commit(state: &mut ShareDialogState) -> ShareDialogAction {
    let Some(subject) = state.subject else {
        state.close();
        return ShareDialogAction::Close;
    };
    let target = state.target_input.trim();
    if Uuid::parse_str(target).is_err() {
        return ShareDialogAction::InvalidTarget;
    }
    let access = state.access.unwrap_or(ShareAccess::Read).label();
    let cmd = match subject {
        ShareSubject::Project(id) => format!("share project {id} {target} --access {access}"),
        ShareSubject::Issue(id) => format!("share issue {id} {target} --access {access}"),
    };
    state.close();
    ShareDialogAction::Enqueue(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn enter_with_valid_uuid_dispatches_share_project() {
        let mut s = ShareDialogState::default();
        let project = Uuid::new_v4();
        let target = Uuid::new_v4();
        s.open_for_project(project);
        for c in target.to_string().chars() {
            handle_key(&mut s, key(KeyCode::Char(c)));
        }
        let act = handle_key(&mut s, key(KeyCode::Enter)).unwrap();
        match act {
            ShareDialogAction::Enqueue(cmd) => {
                assert!(cmd.starts_with(&format!("share project {project} {target}")));
                assert!(cmd.contains("--access read"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(!s.visible);
    }

    #[test]
    fn enter_with_invalid_target_returns_invalid_marker() {
        let mut s = ShareDialogState::default();
        s.open_for_project(Uuid::new_v4());
        for c in "not-a-uuid".chars() {
            handle_key(&mut s, key(KeyCode::Char(c)));
        }
        assert_eq!(
            handle_key(&mut s, key(KeyCode::Enter)),
            Some(ShareDialogAction::InvalidTarget)
        );
        // Dialog stays open so user can fix the input.
        assert!(s.visible);
    }

    #[test]
    fn right_arrow_cycles_access_when_focused() {
        let mut s = ShareDialogState::default();
        s.open_for_project(Uuid::new_v4());
        handle_key(&mut s, key(KeyCode::Tab)); // focus → Access
        handle_key(&mut s, key(KeyCode::Right));
        assert_eq!(s.access, Some(ShareAccess::Write));
        handle_key(&mut s, key(KeyCode::Right));
        assert_eq!(s.access, Some(ShareAccess::Admin));
        handle_key(&mut s, key(KeyCode::Right));
        assert_eq!(s.access, Some(ShareAccess::Read));
    }

    #[test]
    fn tab_only_moves_focus_no_other_side_effects() {
        let mut s = ShareDialogState::default();
        s.open_for_project(Uuid::new_v4());
        assert_eq!(s.focus, Focus::Target);
        handle_key(&mut s, key(KeyCode::Tab));
        assert_eq!(s.focus, Focus::Access);
        handle_key(&mut s, key(KeyCode::Tab));
        assert_eq!(s.focus, Focus::Target);
    }

    #[test]
    fn esc_closes_and_clears() {
        let mut s = ShareDialogState::default();
        s.open_for_issue(Uuid::new_v4());
        s.target_input = "abc".into();
        let act = handle_key(&mut s, key(KeyCode::Esc)).unwrap();
        assert_eq!(act, ShareDialogAction::Close);
        assert!(!s.visible);
        assert!(s.target_input.is_empty());
    }
}

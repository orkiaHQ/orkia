// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//!
//! Single-input modal: paste an invite URL or raw nonce, press Enter
//! to dispatch `$invite accept <nonce>` through the shell.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::theme::Theme;

#[derive(Debug, Clone, Default)]
pub struct InviteModalState {
    pub visible: bool,
    pub input: String,
}

impl InviteModalState {
    pub fn open(&mut self) {
        self.visible = true;
        self.input.clear();
    }
    pub fn close(&mut self) {
        self.visible = false;
        self.input.clear();
    }
    pub fn is_visible(&self) -> bool {
        self.visible
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum InviteModalAction {
    Enqueue(String),
    Close,
}

pub fn render_invite_modal(f: &mut Frame<'_>, area: Rect, state: &InviteModalState, theme: &Theme) {
    if !state.visible {
        return;
    }
    let block = Block::default()
        .title(Span::styled(
            " Accept Invite ",
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(theme.accent),
        ))
        .borders(Borders::ALL);

    let lines = vec![
        Line::from(Span::styled(
            "Paste invite link or nonce:",
            Style::default().fg(theme.dim),
        )),
        Line::from(vec![
            Span::raw("> "),
            Span::styled(
                state.input.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
        ]),
        Line::raw(""),
        Line::styled(
            "[Enter] accept   [Esc] cancel",
            Style::default().fg(theme.dim),
        ),
    ];

    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

pub fn handle_key(state: &mut InviteModalState, key: KeyEvent) -> Option<InviteModalAction> {
    if !state.visible {
        return None;
    }
    match key.code {
        KeyCode::Esc => {
            state.close();
            Some(InviteModalAction::Close)
        }
        KeyCode::Enter => {
            let nonce = extract_nonce_from_input(&state.input);
            state.close();
            if nonce.is_empty() {
                None
            } else {
                Some(InviteModalAction::Enqueue(format!("invite accept {nonce}")))
            }
        }
        KeyCode::Backspace => {
            state.input.pop();
            None
        }
        KeyCode::Char(c) => {
            state.input.push(c);
            None
        }
        _ => None,
    }
}

/// Pull the nonce out of either a magic-link URL (`.../i/<nonce>`)
/// or accept the input verbatim. Trims a trailing slash so paste-with-
/// trailing-slash works.
pub fn extract_nonce_from_input(input: &str) -> String {
    let trimmed = input.trim();
    if let Some(idx) = trimmed.rfind("/i/") {
        let tail = &trimmed[idx + 3..];
        return tail.trim().trim_end_matches('/').to_string();
    }
    trimmed.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn extract_from_url_strips_prefix_and_slash() {
        assert_eq!(
            extract_nonce_from_input("https://orkia.app/i/abc123"),
            "abc123"
        );
        assert_eq!(
            extract_nonce_from_input("https://orkia.app/i/abc123/"),
            "abc123"
        );
        assert_eq!(
            extract_nonce_from_input("  https://orkia.app/i/abc/  "),
            "abc"
        );
    }

    #[test]
    fn extract_from_raw_nonce_round_trips() {
        assert_eq!(extract_nonce_from_input("abc123"), "abc123");
        assert_eq!(extract_nonce_from_input("  abc123 "), "abc123");
    }

    #[test]
    fn typing_then_enter_emits_accept_command() {
        let mut s = InviteModalState::default();
        s.open();
        for c in "abc".chars() {
            handle_key(&mut s, key(KeyCode::Char(c)));
        }
        let act = handle_key(&mut s, key(KeyCode::Enter)).unwrap();
        match act {
            InviteModalAction::Enqueue(cmd) => assert_eq!(cmd, "invite accept abc"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(!s.visible);
    }

    #[test]
    fn empty_input_does_not_dispatch() {
        let mut s = InviteModalState::default();
        s.open();
        assert!(handle_key(&mut s, key(KeyCode::Enter)).is_none());
    }

    #[test]
    fn esc_closes_and_clears() {
        let mut s = InviteModalState::default();
        s.open();
        s.input = "abc".into();
        let act = handle_key(&mut s, key(KeyCode::Esc)).unwrap();
        assert_eq!(act, InviteModalAction::Close);
        assert!(!s.visible);
        assert!(s.input.is_empty());
    }

    #[test]
    fn backspace_pops_last_char() {
        let mut s = InviteModalState::default();
        s.open();
        s.input = "abc".into();
        handle_key(&mut s, key(KeyCode::Backspace));
        assert_eq!(s.input, "ab");
    }
}

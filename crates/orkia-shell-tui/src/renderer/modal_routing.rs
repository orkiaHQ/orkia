// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! `ShellRenderer` trait implementation for `TuiRenderer`.
//!
//! Separated from `mod.rs` to keep the main file within the 600-line
//! module limit (CLAUDE.md). Contains the `ShellRenderer` trait impl
//! (publish / read_line / select_prompt / yield / reclaim / note_exit)
//! plus the keyboard routing and team-modal dispatch helpers.

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use orkia_shell_types::{BlockContent, PromptContext, RenderEvent, ShellRenderer};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::time::Duration;

use super::TuiRenderer;
use crate::app::{InputMode, View};
use crate::widgets::{
    AttentionModalAction, InviteModalAction, ShareDialogAction, TeamDetailAction, TeamPaneAction,
    attention_modal, hex_to_color, invite_modal, render_briefing, share_dialog, team_detail,
    team_pane,
};

pub(super) enum KeyOutcome {
    None,
    Submit(String),
    Eof,
}

impl ShellRenderer for TuiRenderer {
    fn yield_terminal(&mut self) {
        self.yield_to_pty();
    }

    fn reclaim_terminal(&mut self) {
        self.reclaim_from_pty();
        self.draw();
    }

    fn note_exit(&mut self, exit_code: i32) {
        if let Some(card) = self.cards.last_mut() {
            card.exit_code = Some(exit_code);
            card.failed = exit_code != 0;
        }
        if self.refresh_daemon_after_command {
            self.refresh_daemon_after_command = false;
            self.refresh_daemon_snapshot();
        }
        self.draw();
    }

    fn is_attach_capable(&self) -> bool {
        // Widget-mode attach is intentionally disabled — see audit
        // finding P3-001 and the module-level doc above. The REPL
        // routes foregrounding through yield_terminal → raw splice →
        // reclaim_terminal, which avoids all crossterm-driven byte
        // re-encoding.
        false
    }

    fn publish(&mut self, event: RenderEvent) {
        match event {
            RenderEvent::Block(block) => {
                if let BlockContent::Attention { rows, message } = &block {
                    self.attention_modal.open(rows.clone(), message.clone());
                }
                self.push_block(block);
                self.scroll_offset = 0;
            }
            RenderEvent::RoutingInfo {
                agent,
                confidence,
                reason,
            } => {
                self.push_block(BlockContent::SystemInfo(format!(
                    "▸ routed to {agent} ({reason}, {:.0}%)",
                    confidence * 100.0
                )));
                self.scroll_offset = 0;
            }
            RenderEvent::Welcome(info) => {
                let briefing = render_briefing(
                    &info.agents,
                    &self.workspace,
                    info.seal_chain_length,
                    info.last_seal_hash.as_deref(),
                );
                self.agents = info.agents;
                for b in briefing {
                    self.push_block(b);
                }
                self.scroll_offset = 0;
            }
            RenderEvent::Prompt(ctx) => {
                self.cwd = ctx.cwd.clone();
                self.pending_approvals = ctx.pending_approvals;
                self.rfc_scope = ctx.rfc_scope.clone();
            }
            RenderEvent::JobUpdate(_) => {
                // JobsSnapshot carries the authoritative list; this event is informational.
            }
            RenderEvent::JobsSnapshot(jobs) => {
                self.jobs = jobs;
            }
            RenderEvent::WorkspaceSnapshot(ws) => {
                self.workspace = ws;
            }
            RenderEvent::TeamSnapshot(snapshot) => {
                self.team_snapshot = snapshot;
                // Refresh team color in case the current team's color
                // changed in the new snapshot.
                self.current_team_color = self
                    .current_team
                    .and_then(|tid| {
                        self.team_snapshot
                            .teams
                            .iter()
                            .find(|t| t.id == tid)
                            .and_then(|t| t.color.as_deref())
                    })
                    .and_then(hex_to_color);
            }
            RenderEvent::CurrentTeamChanged { team_id, color } => {
                self.current_team = team_id;
                self.current_team_color = color.as_deref().and_then(hex_to_color).or_else(|| {
                    team_id.and_then(|tid| {
                        self.team_snapshot
                            .teams
                            .iter()
                            .find(|t| t.id == tid)
                            .and_then(|t| t.color.as_deref())
                            .and_then(hex_to_color)
                    })
                });
            }
        }
        self.draw();
    }

    fn read_line(&mut self, ctx: &PromptContext) -> Option<String> {
        self.cwd = ctx.cwd.clone();
        self.pending_approvals = ctx.pending_approvals;
        self.rfc_scope = ctx.rfc_scope.clone();
        // The prompt is back: the previous command's output is done, so
        // stamp its card finished before reading the next line.
        self.close_current_card();
        self.refresh_daemon_snapshot();
        // before this read. Acts as a one-line "macro" mechanism so
        // modal Enter immediately submits a builtin invocation.
        if let Some(cmd) = self.injected_commands.pop_front() {
            self.open_card(&cmd);
            return Some(cmd);
        }
        self.input.clear();
        self.draw();

        loop {
            match event::poll(Duration::from_millis(150)) {
                Ok(true) => {}
                Ok(false) => {
                    // Periodic redraw to refresh any state that changed via publish().
                    self.draw();
                    continue;
                }
                // A signal (SIGWINCH/SIGCHLD) can interrupt the underlying
                // poll; that's transient, not EOF — retry instead of killing
                // the 24/7 session (BUG-042).
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
            let ev = match event::read() {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            };
            match ev {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    match self.handle_key(key) {
                        KeyOutcome::None => {}
                        KeyOutcome::Submit(line) => {
                            self.open_card(&line);
                            return Some(line);
                        }
                        KeyOutcome::Eof => return None,
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
            self.draw();
        }
    }

    fn select_prompt(
        &mut self,
        title: &str,
        detail: &str,
        options: &[&str],
        default: usize,
    ) -> Option<usize> {
        let mut sel = default.min(options.len().saturating_sub(1));
        loop {
            self.draw_select(title, detail, options, sel);
            // Mirror `read_line`: poll with a timeout so pending `publish()`
            // state still redraws while the modal is open, and a transient
            // signal interruption retries instead of reading as "cancelled"
            // (BUG-105 / BUG-042).
            match event::poll(Duration::from_millis(150)) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
            let ev = match event::read() {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            };
            let Event::Key(k) = ev else { continue };
            if k.kind == KeyEventKind::Release {
                continue;
            }
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    if sel + 1 < options.len() {
                        sel += 1;
                    }
                }
                // First-letter / number pre-selection so a typed `y`/`1`
                // (or a scripted harness) lands on the right option.
                KeyCode::Char('y') | KeyCode::Char('Y') => sel = 0,
                KeyCode::Char('n') | KeyCode::Char('N') => sel = options.len().saturating_sub(1),
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    let i = (c as usize).wrapping_sub('1' as usize);
                    if i < options.len() {
                        sel = i;
                    }
                }
                KeyCode::Enter => return Some(sel),
                KeyCode::Esc => return None,
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    return None;
                }
                _ => {}
            }
        }
    }
}

impl TuiRenderer {
    /// Render the [`ShellRenderer::select_prompt`] menu: a centred modal
    /// with the title, a dim detail line, the numbered options (the
    /// selected one marked `❯` and highlighted), and a key hint.
    pub(super) fn draw_select(&mut self, title: &str, detail: &str, options: &[&str], sel: usize) {
        if let Err(e) = self.terminal.draw(|f| {
            let height = (options.len() + 7).clamp(7, 20) as u16;
            let area = super::centered(f.area(), 78, height);
            let mut lines: Vec<Line> = Vec::new();
            if !detail.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!("  {detail}"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            lines.push(Line::from(""));
            for (i, opt) in options.iter().enumerate() {
                let selected = i == sel;
                let marker = if selected { "❯ " } else { "  " };
                let style = if selected {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                lines.push(Line::from(Span::styled(
                    format!("  {marker}{}. {opt}", i + 1),
                    style,
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  ↑/↓ to move · Enter to confirm · Esc to cancel",
                Style::default().fg(Color::DarkGray),
            )));
            let block = Block::default().borders(Borders::ALL).title(Span::styled(
                format!(" {title} "),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            f.render_widget(Clear, area);
            f.render_widget(Paragraph::new(lines).block(block), area);
        }) {
            tracing::warn!(error = %e, "tui draw failed");
        }
    }

    /// Route a key through the active team-mode modal stack. Returns
    /// `Some(outcome)` when the modal consumed the key (whether or
    /// not it produced a side effect); `None` when no modal is open.
    /// Modals take priority so global shortcuts don't leak.
    pub(super) fn handle_team_modal_key(&mut self, key: KeyEvent) -> Option<KeyOutcome> {
        if self.attention_modal.is_visible() {
            if let Some(act) = attention_modal::handle_key(&mut self.attention_modal, key) {
                return Some(match act {
                    AttentionModalAction::Enqueue(cmd) => KeyOutcome::Submit(cmd),
                    AttentionModalAction::Close => KeyOutcome::None,
                });
            }
            return Some(KeyOutcome::None);
        }
        if self.invite_modal.is_visible() {
            if let Some(act) = invite_modal::handle_key(&mut self.invite_modal, key) {
                self.apply_invite_action(act);
            }
            return Some(KeyOutcome::None);
        }
        if self.share_dialog.is_visible() {
            if let Some(act) = share_dialog::handle_key(&mut self.share_dialog, key) {
                self.apply_share_action(act);
            }
            return Some(KeyOutcome::None);
        }
        if self.team_detail.is_visible() {
            if let Some(act) = team_detail::handle_key(&mut self.team_detail, key) {
                self.apply_detail_action(act);
            }
            return Some(KeyOutcome::None);
        }
        if self.team_pane.is_visible() {
            if let Some(act) = team_pane::handle_key(
                &mut self.team_pane,
                &self.team_snapshot,
                self.current_team,
                key,
            ) {
                self.apply_pane_action(act);
            }
            return Some(KeyOutcome::None);
        }
        None
    }

    pub(super) fn apply_pane_action(&mut self, action: TeamPaneAction) {
        match action {
            TeamPaneAction::OpenDetail(id) => self.team_detail.open(id),
            TeamPaneAction::Enqueue(cmd) => self.injected_commands.push_back(cmd),
            TeamPaneAction::Close => {}
        }
    }

    pub(super) fn apply_detail_action(&mut self, action: TeamDetailAction) {
        match action {
            TeamDetailAction::Enqueue(cmd) => self.injected_commands.push_back(cmd),
            TeamDetailAction::Close => {}
        }
    }

    pub(super) fn apply_invite_action(&mut self, action: InviteModalAction) {
        match action {
            InviteModalAction::Enqueue(cmd) => self.injected_commands.push_back(cmd),
            InviteModalAction::Close => {}
        }
    }

    pub(super) fn apply_share_action(&mut self, action: ShareDialogAction) {
        match action {
            ShareDialogAction::Enqueue(cmd) => self.injected_commands.push_back(cmd),
            ShareDialogAction::InvalidTarget | ShareDialogAction::Close => {}
        }
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> KeyOutcome {
        if let Some(outcome) = self.handle_team_modal_key(key) {
            return outcome;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // input-char fallthrough so Ctrl-T doesn't drop a literal 't'
        // into the prompt. The editing bindings (a/e/w/u) live here too
        // so they never leak through as inserted characters.
        if ctrl {
            match key.code {
                KeyCode::Char('t') => {
                    self.team_pane.toggle();
                    return KeyOutcome::None;
                }
                KeyCode::Char('i') if shift => {
                    self.invite_modal.open();
                    return KeyOutcome::None;
                }
                KeyCode::Char('o') => {
                    self.toggle_selected_fold();
                    return KeyOutcome::None;
                }
                KeyCode::Char('g') => {
                    return KeyOutcome::Submit("attention pull".into());
                }
                _ => {}
            }
        }
        if self.app.mode != InputMode::Normal {
            return self.handle_buffer_key(key);
        }
        if let Some(outcome) = self.handle_daemon_key(key) {
            return outcome;
        }
        match key.code {
            KeyCode::Char('b') if ctrl => {
                self.layout.toggle_sidebar();
                KeyOutcome::None
            }
            KeyCode::Char('d') if ctrl => KeyOutcome::Eof,
            KeyCode::Char('q') => KeyOutcome::Eof,
            KeyCode::Esc => {
                if self.app.detail_open {
                    self.app.detail_open = false;
                } else if !self.app.filter.is_empty() {
                    self.app.clear_filter();
                } else {
                    self.selected = None;
                }
                KeyOutcome::None
            }
            KeyCode::Tab => {
                self.app.next_view();
                KeyOutcome::None
            }
            KeyCode::BackTab => {
                self.app.prev_view();
                KeyOutcome::None
            }
            KeyCode::Char('?') => {
                self.app.set_view(View::Help);
                KeyOutcome::None
            }
            KeyCode::Char('/') => {
                self.app.begin_filter();
                KeyOutcome::None
            }
            KeyCode::Char(':') => {
                self.app.begin_command();
                KeyOutcome::None
            }
            KeyCode::Char('r') => {
                let cmd = self.app.view.command();
                if cmd.is_empty() {
                    KeyOutcome::None
                } else {
                    KeyOutcome::Submit(cmd.into())
                }
            }
            KeyCode::Char('o') if self.app.view == View::Output => {
                self.open_selected_source();
                KeyOutcome::None
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let index = (c as usize).saturating_sub('1' as usize);
                if let Some(view) = View::ALL.get(index) {
                    self.app.set_view(*view);
                }
                KeyOutcome::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.app.view == View::Output && alt {
                    self.select_prev_card();
                    self.app.source_selected = 0;
                } else if self.app.view == View::Output && self.app.detail_open {
                    self.app.move_source_up();
                } else {
                    self.app.move_up();
                }
                KeyOutcome::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.app.view == View::Output && alt {
                    self.select_next_card();
                    self.app.source_selected = 0;
                } else if self.app.view == View::Output && self.app.detail_open {
                    let n = self.selected_source_refs().len();
                    self.app.move_source_down(n);
                } else {
                    let n = self.app.visible_len(
                        &self.agents,
                        &self.jobs,
                        &self.daemon_jobs,
                        &self.workspace,
                    );
                    self.app.move_down(n);
                }
                KeyOutcome::None
            }
            // Swallow Ctrl-Z in shell mode. In raw mode the kernel won't send
            // SIGTSTP, but during yield/reclaim transitions the terminal is
            // briefly cooked — if Ctrl-Z arrived then it would suspend orkia
            // and strand the user's parent shell in a half-restored state.
            KeyCode::Char('z') if ctrl => KeyOutcome::None,
            KeyCode::Enter => {
                self.app.detail_open = !self.app.detail_open;
                KeyOutcome::None
            }
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_add(5);
                KeyOutcome::None
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(5);
                KeyOutcome::None
            }
            _ => KeyOutcome::None,
        }
    }

    fn handle_buffer_key(&mut self, key: KeyEvent) -> KeyOutcome {
        match key.code {
            KeyCode::Esc => {
                self.app.cancel_mode();
                KeyOutcome::None
            }
            KeyCode::Backspace => {
                self.app.backspace();
                KeyOutcome::None
            }
            KeyCode::Enter => self.submit_buffer(),
            KeyCode::Char('y') | KeyCode::Char('Y') if self.app.mode == InputMode::ConfirmKill => {
                let cmd = self.app.selected_kill_command(&self.daemon_jobs);
                self.app.cancel_mode();
                cmd.map_or(KeyOutcome::None, |cmd| {
                    self.submit_daemon_command(cmd, true)
                })
            }
            KeyCode::Char('n') | KeyCode::Char('N') if self.app.mode == InputMode::ConfirmKill => {
                self.app.cancel_mode();
                KeyOutcome::None
            }
            KeyCode::Char(c) => {
                self.app.push_char(c);
                KeyOutcome::None
            }
            _ => KeyOutcome::None,
        }
    }

    fn submit_buffer(&mut self) -> KeyOutcome {
        match self.app.mode {
            InputMode::Filter => {
                self.app.mode = InputMode::Normal;
                self.app.buffer.clear();
                KeyOutcome::None
            }
            InputMode::Command => {
                let line = self.app.buffer.trim().to_string();
                self.app.cancel_mode();
                if line.is_empty() {
                    KeyOutcome::None
                } else {
                    KeyOutcome::Submit(line)
                }
            }
            InputMode::Tell => {
                let body = self.app.buffer.trim().to_string();
                let cmd = self.app.selected_tell_command(&self.daemon_jobs, &body);
                self.app.cancel_mode();
                cmd.map_or(KeyOutcome::None, |cmd| {
                    self.submit_daemon_command(cmd, true)
                })
            }
            InputMode::ConfirmKill => KeyOutcome::None,
            InputMode::Normal => KeyOutcome::None,
        }
    }
}

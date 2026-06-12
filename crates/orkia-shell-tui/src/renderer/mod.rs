// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! `TuiRenderer` — implements [`ShellRenderer`] using ratatui + crossterm.
//!
//! ## Attach mode
//!
//! Until a structurally-sound in-TUI PTY-widget design lands (see audit
//! P3-001), the TUI renderer advertises `is_attach_capable() = false`
//! and the REPL routes foregrounding through the yield/reclaim path,
//! which uses `orkia-shell::job::raw_attach::run_foreground` — a raw
//! byte splice that satisfies CLAUDE.md "no band-aids on structural
//! problems". The earlier crossterm-event-driven `drive_attached_loop`
//! has been removed because it parsed terminal capability replies (iTerm
//! DCS, kitty kbd, focus events, bracketed paste) as fake keystrokes
//! and re-encoded them as PTY bytes, corrupting the agent's input. The
//! `AttachedJob` snapshotting infrastructure in `attached.rs` is
//! preserved for the future re-design.

use crate::app::TuiApp;
use crate::card::CommandCard;
use crate::daemon::{DaemonJobRow, parse_ps_json};
use crate::input::Input;
use crate::layout::ShellLayout;
use crate::theme::Theme;
use crate::widgets::{
    AttentionModalState, CockpitModel, InviteModalState, ShareDialogState, TeamDetailState,
    TeamPaneState, render_attention_modal, render_cockpit, render_invite_modal,
    render_share_dialog, render_team_detail, render_team_pane,
};

use crossterm::{execute, terminal};
use orkia_shell_types::{AgentInfo, BlockContent, JobInfo, TeamSnapshot, Workspace};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Block;
use std::collections::VecDeque;
use std::io::{self, Stdout, Write};
use std::time::Instant;
use uuid::Uuid;

pub struct TuiRenderer {
    pub(crate) terminal: Terminal<CrosstermBackend<Stdout>>,
    pub(crate) layout: ShellLayout,
    pub(crate) theme: Theme,

    /// Command output grouped into Warp-style cards. Always holds at
    /// least the preamble card, so `last_mut()` never returns `None`.
    pub(crate) cards: Vec<CommandCard>,
    pub(crate) scroll_offset: usize,
    /// Index into `cards` of the selected card (for fold/navigation), or
    /// `None` when focus is on the input and the view is bottom-pinned.
    pub(crate) selected: Option<usize>,
    pub(crate) input: Input,

    pub(crate) agents: Vec<AgentInfo>,
    pub(crate) jobs: Vec<JobInfo>,
    pub(crate) daemon_jobs: Vec<DaemonJobRow>,
    pub(crate) daemon_status: String,
    pub(crate) daemon_panel_title: String,
    pub(crate) daemon_panel_lines: Vec<String>,
    pub(crate) refresh_daemon_after_command: bool,
    pub(crate) workspace: Workspace,
    pub(crate) pending_approvals: usize,
    pub(crate) cwd: String,
    pub(crate) rfc_scope: Option<orkia_shell_types::RfcScopeSegment>,
    /// Snapshot is updated via `RenderEvent::TeamSnapshot`; the
    /// modals own their own visible/input state.
    pub(crate) team_snapshot: TeamSnapshot,
    pub(crate) current_team: Option<Uuid>,
    pub(crate) current_team_color: Option<Color>,
    pub(crate) team_pane: TeamPaneState,
    pub(crate) team_detail: TeamDetailState,
    pub(crate) invite_modal: InviteModalState,
    pub(crate) share_dialog: ShareDialogState,
    pub(crate) attention_modal: AttentionModalState,
    pub(crate) app: TuiApp,
    /// Commands the widgets injected; consumed by `read_line` before
    /// it polls crossterm for user input. Keeps the renderer trait
    /// surface unchanged (no separate dispatch channel).
    pub(crate) injected_commands: VecDeque<String>,
}

mod daemon_keys;
mod modal_routing;

impl TuiRenderer {
    /// Enter alternate screen + raw mode and build the terminal.
    pub fn new(agents: Vec<AgentInfo>, workspace: Workspace) -> io::Result<Self> {
        install_panic_hook();
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, terminal::EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self {
            terminal,
            layout: ShellLayout::new(),
            theme: Theme::default(),
            cards: vec![CommandCard::preamble()],
            scroll_offset: 0,
            selected: None,
            input: Input::default(),
            agents,
            jobs: Vec::new(),
            daemon_jobs: Vec::new(),
            daemon_status: "unknown".to_string(),
            daemon_panel_title: String::new(),
            daemon_panel_lines: Vec::new(),
            refresh_daemon_after_command: false,
            workspace,
            pending_approvals: 0,
            cwd: String::from("~"),
            rfc_scope: None,
            team_snapshot: TeamSnapshot::default(),
            current_team: None,
            current_team_color: None,
            team_pane: TeamPaneState::default(),
            team_detail: TeamDetailState::default(),
            invite_modal: InviteModalState::default(),
            share_dialog: ShareDialogState::default(),
            attention_modal: AttentionModalState::default(),
            app: TuiApp::default(),
            injected_commands: VecDeque::new(),
        })
    }

    fn yield_to_pty(&mut self) {
        let _ = execute!(io::stdout(), terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }

    fn reclaim_from_pty(&mut self) {
        let _ = terminal::enable_raw_mode();
        let _ = execute!(io::stdout(), terminal::EnterAlternateScreen);
        let _ = self.terminal.clear();
    }

    /// Append a block to the currently-open card. The card vec always
    /// holds at least the preamble, so `last_mut` is never `None`.
    fn push_block(&mut self, block: BlockContent) {
        if let Some(card) = self.cards.last_mut() {
            card.push_block(block);
        }
    }

    /// Stamp the open command card finished (prompt returned).
    fn close_current_card(&mut self) {
        if let Some(card) = self.cards.last_mut() {
            card.close(Instant::now());
        }
    }

    /// Open a fresh card for a submitted command. Blank lines don't open a
    /// card — they just re-prompt — so empty output never grows the list.
    fn open_card(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        self.cards
            .push(CommandCard::command(line.to_string(), Instant::now()));
        self.scroll_offset = 0;
        self.selected = None;
    }

    /// Move the selection one command card toward the top. From no
    /// selection this lands on the last (most recent) command card.
    fn select_prev_card(&mut self) {
        let start = self.selected.unwrap_or(self.cards.len());
        self.selected = self.cards[..start.min(self.cards.len())]
            .iter()
            .enumerate()
            .rev()
            .find(|(_, c)| c.command.is_some())
            .map(|(i, _)| i);
    }

    /// Move the selection one command card toward the bottom; past the
    /// last card it clears, returning focus to the input.
    fn select_next_card(&mut self) {
        let Some(cur) = self.selected else { return };
        self.selected = self
            .cards
            .iter()
            .enumerate()
            .skip(cur + 1)
            .find(|(_, c)| c.command.is_some())
            .map(|(i, _)| i);
    }

    /// Fold / unfold the selected card's body.
    fn toggle_selected_fold(&mut self) {
        if let Some(i) = self.selected
            && let Some(card) = self.cards.get_mut(i)
        {
            card.collapsed = !card.collapsed;
        }
    }

    fn draw(&mut self) {
        let layout = &self.layout;
        let theme = &self.theme;
        let cards = &self.cards;
        let scroll = self.scroll_offset;
        let selected = self.selected;
        let agents = &self.agents;
        let jobs = &self.jobs;
        let daemon_jobs = &self.daemon_jobs;
        let daemon_status = self.daemon_status.as_str();
        let daemon_panel_title = self.daemon_panel_title.as_str();
        let daemon_panel_lines = &self.daemon_panel_lines;
        let workspace = &self.workspace;
        let pending = self.pending_approvals;
        let cwd = self.cwd.as_str();
        let team_color = self.current_team_color;
        let team_snapshot = &self.team_snapshot;
        let current_team = self.current_team;
        let team_pane = &self.team_pane;
        let team_detail = &self.team_detail;
        let invite_modal = &self.invite_modal;
        let share_dialog = &self.share_dialog;
        let attention_modal = &self.attention_modal;
        let app = &self.app;

        if let Err(e) = self.terminal.draw(|f| {
            let rects = layout.compute(f.area());

            // Paint the base background first so every zone sits on the
            // theme's surface instead of the terminal's default colour.
            f.render_widget(
                Block::default().style(Style::default().bg(theme.bg)),
                f.area(),
            );

            // left of the main pane when `current_team` is set and
            // the team has a color. Falls through to the default
            // render when no team / no color.
            let content_area = match rects.sidebar {
                Some(sidebar) => Rect {
                    x: sidebar.x,
                    y: sidebar.y,
                    width: sidebar.width.saturating_add(rects.main.width),
                    height: rects.main.height,
                },
                None => rects.main,
            };
            let main_area = match team_color {
                Some(color) => {
                    let bar = Rect {
                        x: content_area.x,
                        y: content_area.y,
                        width: 2.min(content_area.width),
                        height: content_area.height,
                    };
                    let block = Block::default().style(Style::default().bg(color));
                    f.render_widget(block, bar);
                    Rect {
                        x: content_area.x + bar.width,
                        y: content_area.y,
                        width: content_area.width.saturating_sub(bar.width),
                        height: content_area.height,
                    }
                }
                None => content_area,
            };
            let cockpit = CockpitModel {
                app,
                agents,
                jobs,
                daemon_jobs,
                daemon_status,
                daemon_panel_title,
                daemon_panel_lines,
                workspace,
                team_snapshot,
                cards,
                pending_approvals: pending,
                cwd,
                sidebar_visible: layout.sidebar_visible,
                scroll_offset: scroll,
                selected_card: selected,
            };
            render_cockpit(f, main_area, &cockpit, theme);

            // Modals render last so they paint over everything.
            // Centered, fixed-ish dimensions clipped to the frame.
            if team_pane.is_visible() {
                let area = centered(f.area(), 60, 18);
                render_team_pane(f, area, team_pane, team_snapshot, current_team, theme);
            }
            if team_detail.is_visible() {
                let area = centered(f.area(), 70, 22);
                render_team_detail(f, area, team_detail, team_snapshot, theme);
            }
            if invite_modal.is_visible() {
                let area = centered(f.area(), 60, 9);
                render_invite_modal(f, area, invite_modal, theme);
            }
            if share_dialog.is_visible() {
                let area = centered(f.area(), 70, 12);
                render_share_dialog(f, area, share_dialog, theme);
            }
            if attention_modal.is_visible() {
                let area = centered(f.area(), 76, 18);
                render_attention_modal(f, area, attention_modal, theme);
            }
        }) {
            tracing::warn!(error = %e, "tui draw failed");
        }
    }

    pub(crate) fn refresh_daemon_snapshot(&mut self) {
        match load_daemon_jobs() {
            Ok(jobs) => {
                self.daemon_jobs = jobs;
                self.daemon_status = load_daemon_status();
                self.daemon_panel_title.clear();
                self.daemon_panel_lines.clear();
                let n =
                    crate::daemon::visible_daemon_rows(&self.daemon_jobs, &self.app.filter).len();
                if self.app.view == crate::app::View::Jobs {
                    self.app.clamp_selection(n);
                }
            }
            Err(err) => {
                self.daemon_status = format!("error: {err}");
                self.daemon_jobs.clear();
                self.daemon_panel_title.clear();
                self.daemon_panel_lines.clear();
                if self.app.view == crate::app::View::Jobs {
                    self.app.clamp_selection(0);
                }
            }
        }
    }

    pub(crate) fn load_daemon_panel(&mut self, command: &str, title: &str) {
        match run_public_command(command) {
            Ok(lines) => {
                self.daemon_panel_title = title.to_string();
                self.daemon_panel_lines = lines;
                self.scroll_offset = 0;
                self.app.detail_open = true;
                self.app.status = format!("{title} loaded");
            }
            Err(err) => {
                self.daemon_panel_title = "error".to_string();
                self.daemon_panel_lines = vec![err];
                self.scroll_offset = 0;
                self.app.detail_open = true;
            }
        }
    }

    pub(crate) fn open_selected_source(&mut self) {
        let refs = self.selected_source_refs();
        self.app.clamp_source_selection(refs.len());
        let Some(source_ref) = self.selected_source_ref() else {
            self.app.status = "no citation/source ref on selected output card".into();
            return;
        };
        let command = source_open_command(&source_ref);
        self.load_daemon_panel(&command, "source");
    }

    fn selected_source_ref(&self) -> Option<String> {
        let refs = self.selected_source_refs();
        refs.get(self.app.source_selected)
            .cloned()
            .or_else(|| refs.first().cloned())
    }

    pub(crate) fn selected_source_refs(&self) -> Vec<String> {
        let card = self
            .selected
            .and_then(|idx| self.cards.get(idx))
            .or_else(|| self.cards.iter().rev().find(|card| card.command.is_some()));
        let Some(card) = card else {
            return Vec::new();
        };
        crate::source_refs::refs_from_blocks(&card.blocks)
    }

    pub(in crate::renderer) fn submit_daemon_command(
        &mut self,
        command: String,
        refresh_after: bool,
    ) -> self::modal_routing::KeyOutcome {
        self.refresh_daemon_after_command = refresh_after;
        self::modal_routing::KeyOutcome::Submit(command)
    }
}

fn load_daemon_jobs() -> Result<Vec<DaemonJobRow>, String> {
    let out = std::process::Command::new(current_exe()?)
        .args(["ps", "--json"])
        .output()
        .map_err(|e| format!("ps --json: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let raw = String::from_utf8(out.stdout).map_err(|e| format!("ps --json utf8: {e}"))?;
    parse_ps_json(&raw).map_err(|e| format!("ps --json parse: {e}"))
}

fn load_daemon_status() -> String {
    let Ok(exe) = current_exe() else {
        return "unknown".to_string();
    };
    match std::process::Command::new(exe)
        .args(["daemon", "status"])
        .output()
    {
        Ok(out) if out.status.success() => parse_status_line(&out.stdout),
        Ok(out) => String::from_utf8_lossy(&out.stderr).trim().to_string(),
        Err(err) => format!("status: {err}"),
    }
}

fn run_public_command(command: &str) -> Result<Vec<String>, String> {
    let mut parts = command.split_whitespace();
    let Some(first) = parts.next() else {
        return Err("empty daemon command".to_string());
    };
    let out = std::process::Command::new(current_exe()?)
        .arg(first)
        .args(parts)
        .output()
        .map_err(|e| format!("{command}: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(ToString::to_string)
        .collect())
}

fn source_open_command(source_ref: &str) -> String {
    format!("operator open {source_ref}")
}

fn current_exe() -> Result<std::path::PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("current_exe: {e}"))
}

fn parse_status_line(stdout: &[u8]) -> String {
    let text = String::from_utf8_lossy(stdout);
    let state = status_value(&text, "state").unwrap_or("unknown");
    let pid = status_value(&text, "pid").unwrap_or("-");
    let jobs = status_value(&text, "jobs").unwrap_or("0");
    format!("{state} pid={pid} jobs={jobs}")
}

fn status_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}: ");
    text.lines().find_map(|line| line.strip_prefix(&prefix))
}

/// Centered rect with `desired` width/height clipped to `parent`.
/// Falls back to the full parent when the request doesn't fit.
fn centered(parent: Rect, desired_w: u16, desired_h: u16) -> Rect {
    let w = desired_w.min(parent.width);
    let h = desired_h.min(parent.height);
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

impl Drop for TuiRenderer {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Best-effort terminal teardown. Safe to call repeatedly. Used by `Drop`
/// and by the panic hook so a crash mid-frame doesn't strand the user's
/// shell in raw mode / the alternate screen.
fn restore_terminal() {
    let _ = terminal::disable_raw_mode();
    // `?1049l` exits the alt screen; `?25h` re-shows the cursor (in case
    // we hid it). `sgr0` resets colors; `\r` returns the cursor to col 0.
    let _ = io::stdout().write_all(b"\x1b[?1049l\x1b[?25h\x1b[0m\r");
    let _ = io::stdout().flush();
}

/// Install a panic hook that restores the terminal before the default
/// handler prints the panic message. Without this, a panic inside ratatui
/// leaves the user's parent shell in raw mode / alt screen.
fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            prev(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::{parse_status_line, source_open_command};

    #[test]
    fn daemon_status_summary_includes_state_pid_and_jobs() {
        let summary = parse_status_line(b"state: running\nprotocol_version: 1\npid: 42\njobs: 3\n");
        assert_eq!(summary, "running pid=42 jobs=3");
    }

    #[test]
    fn source_open_command_targets_operator_open() {
        assert_eq!(
            source_open_command("journal://event/7"),
            "operator open journal://event/7"
        );
    }
}

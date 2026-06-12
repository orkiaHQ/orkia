// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use crate::app::{InputMode, TuiApp, View, visible_agents};
use crate::card::CommandCard;
use crate::daemon::DaemonJobRow;
use crate::theme::Theme;
use crate::widgets::cockpit_daemon::{daemon_job_lines, selected_daemon_detail};
use crate::widgets::main_pane::render_main_pane;
use crate::widgets::source_detail::selected_output_detail;
use orkia_shell_types::{AgentInfo, AgentStatus, JobInfo, TeamSnapshot, Workspace};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

pub struct CockpitModel<'a> {
    pub app: &'a TuiApp,
    pub agents: &'a [AgentInfo],
    pub jobs: &'a [JobInfo],
    pub daemon_jobs: &'a [DaemonJobRow],
    pub daemon_status: &'a str,
    pub daemon_panel_title: &'a str,
    pub daemon_panel_lines: &'a [String],
    pub workspace: &'a Workspace,
    pub team_snapshot: &'a TeamSnapshot,
    pub cards: &'a [CommandCard],
    pub pending_approvals: usize,
    pub cwd: &'a str,
    pub sidebar_visible: bool,
    pub scroll_offset: usize,
    pub selected_card: Option<usize>,
}

pub fn render_cockpit(f: &mut Frame<'_>, area: Rect, model: &CockpitModel<'_>, theme: &Theme) {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(area);

    render_top_bar(f, vertical[0], model, theme);
    render_body(f, vertical[1], model, theme);
    render_command_bar(f, vertical[2], model.app, theme);
}

fn render_top_bar(f: &mut Frame<'_>, area: Rect, model: &CockpitModel<'_>, theme: &Theme) {
    let filter = if model.app.filter.is_empty() {
        "filter: -".to_string()
    } else {
        format!("filter: {}", model.app.filter)
    };
    let line = Line::from(vec![
        Span::styled(
            " Orkia ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(model.app.view.label(), Style::default().fg(theme.fg)),
        Span::styled("  ", Style::default().fg(theme.dim)),
        Span::styled(model.cwd.to_string(), Style::default().fg(theme.dim)),
        Span::styled("  ", Style::default().fg(theme.dim)),
        Span::styled(filter, Style::default().fg(theme.dim)),
        Span::styled("  daemon:", Style::default().fg(theme.dim)),
        Span::styled(
            model.daemon_status.to_string(),
            Style::default().fg(theme.yellow),
        ),
        Span::styled("  approvals:", Style::default().fg(theme.dim)),
        Span::styled(
            model.pending_approvals.to_string(),
            Style::default().fg(theme.yellow),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_body(f: &mut Frame<'_>, area: Rect, model: &CockpitModel<'_>, theme: &Theme) {
    if model.app.view == View::Output {
        if model.app.detail_open && area.width >= 76 {
            let split = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(58), Constraint::Min(28)])
                .split(area);
            render_main_pane(
                f,
                split[0],
                model.cards,
                model.scroll_offset,
                model.selected_card,
                theme,
            );
            render_detail(f, split[1], model, theme);
            return;
        }
        render_main_pane(
            f,
            area,
            model.cards,
            model.scroll_offset,
            model.selected_card,
            theme,
        );
        return;
    }

    let split = if model.sidebar_visible && area.width >= 76 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(20),
                Constraint::Percentage(if model.app.detail_open { 52 } else { 100 }),
                Constraint::Min(if model.app.detail_open { 24 } else { 0 }),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(0),
                Constraint::Percentage(100),
                Constraint::Length(0),
            ])
            .split(area)
    };

    if split[0].width > 0 {
        render_nav(f, split[0], model, theme);
    }
    render_resource_view(f, split[1], model, theme);
    if model.app.detail_open && split[2].width > 0 {
        render_detail(f, split[2], model, theme);
    }
}

fn render_nav(f: &mut Frame<'_>, area: Rect, model: &CockpitModel<'_>, theme: &Theme) {
    let mut lines = Vec::new();
    for (idx, view) in View::ALL.iter().enumerate() {
        let count = view_count(*view, model);
        let selected = *view == model.app.view;
        let style = if selected {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.dim)
        };
        let marker = if selected { ">" } else { " " };
        lines.push(Line::styled(
            format!("{marker} {} {:<10} {count}", idx + 1, view.label()),
            style,
        ));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(" views ");
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_resource_view(f: &mut Frame<'_>, area: Rect, model: &CockpitModel<'_>, theme: &Theme) {
    let (title, mut lines) = match model.app.view {
        View::Agents => (" agents ", agent_lines(model, theme)),
        View::Jobs => (
            " daemon jobs ",
            daemon_job_lines(model.app, model.daemon_jobs, theme),
        ),
        View::Approvals => (" approvals ", approval_lines(model, theme)),
        View::Journal => (
            " journal ",
            command_backed_lines("journal", "Press r to run journal", theme),
        ),
        View::Seal => (
            " seal ",
            command_backed_lines("seal --verify", "Press r to verify SEAL", theme),
        ),
        View::Rfcs => (" rfcs ", rfc_lines(model, theme)),
        View::Projects => (" projects ", project_lines(model, theme)),
        View::Teams => (" teams ", team_lines(model, theme)),
        View::Help => (" help ", help_lines(theme)),
        View::Output => (" output ", Vec::new()),
    };
    if lines.is_empty() {
        lines.push(Line::styled("(empty)", Style::default().fg(theme.dim)));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(title);
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_detail(f: &mut Frame<'_>, area: Rect, model: &CockpitModel<'_>, theme: &Theme) {
    let lines = match model.app.view {
        View::Agents => selected_agent_detail(model, theme),
        View::Jobs if !model.daemon_panel_lines.is_empty() => daemon_panel_detail(model, theme),
        View::Jobs => selected_daemon_detail(model.app, model.daemon_jobs, theme),
        View::Output if !model.daemon_panel_lines.is_empty() => daemon_panel_detail(model, theme),
        View::Output => selected_output_detail(model, theme),
        View::Projects => selected_project_detail(model, theme),
        View::Teams => selected_team_detail(model, theme),
        _ => vec![
            Line::styled(
                "No structured detail for this view.",
                Style::default().fg(theme.dim),
            ),
            Line::styled(
                "Press r to run the backing builtin.",
                Style::default().fg(theme.dim),
            ),
        ],
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(" describe ");
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn daemon_panel_detail(model: &CockpitModel<'_>, theme: &Theme) -> Vec<Line<'static>> {
    let title = if model.daemon_panel_title.is_empty() {
        "daemon"
    } else {
        model.daemon_panel_title
    };
    let mut lines = vec![Line::styled(
        title.to_string(),
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )];
    lines.extend(
        model
            .daemon_panel_lines
            .iter()
            .skip(model.scroll_offset)
            .take(200)
            .map(|line| Line::styled(line.clone(), Style::default().fg(theme.fg))),
    );
    lines
}

fn render_command_bar(f: &mut Frame<'_>, area: Rect, app: &TuiApp, theme: &Theme) {
    let prompt = match app.mode {
        InputMode::Normal => "keys",
        InputMode::Filter => "/",
        InputMode::Command => ":",
        InputMode::Tell => "tell",
        InputMode::ConfirmKill => "kill?",
    };
    let text = if app.mode == InputMode::Normal {
        "Tab views | j/k select | / filter | Enter detail | a attach | t tell | s stop | K kill | w wait | i inspect | l logs | o source | g gc | Pg scroll | q quit".to_string()
    } else {
        app.buffer.clone()
    };
    let line = Line::from(vec![
        Span::styled(format!(" {prompt} "), Style::default().fg(theme.accent)),
        Span::styled(text, Style::default().fg(theme.fg)),
        Span::styled("  ", Style::default().fg(theme.dim)),
        Span::styled(app.status.clone(), Style::default().fg(theme.dim)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn agent_lines(model: &CockpitModel<'_>, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        "NAME          STATE      MODEL        ARCHETYPE",
        Style::default().fg(theme.dim).add_modifier(Modifier::BOLD),
    )];
    for (idx, agent) in visible_agents(model.agents, &model.app.filter)
        .iter()
        .enumerate()
    {
        let selected = idx == model.app.selected;
        let state = agent_status_label(&agent.status);
        lines.push(row(
            selected,
            format!(
                "{:<13} {:<10} {:<12} {}",
                truncate(&agent.name, 13),
                state,
                truncate(&agent.model, 12),
                truncate(&agent.archetype, 24)
            ),
            theme,
        ));
    }
    lines
}

fn approval_lines(model: &CockpitModel<'_>, theme: &Theme) -> Vec<Line<'static>> {
    if model.pending_approvals == 0 {
        return vec![Line::styled(
            "No pending approvals.",
            Style::default().fg(theme.dim),
        )];
    }
    vec![
        Line::styled(
            format!("{} approval(s) pending", model.pending_approvals),
            Style::default()
                .fg(theme.yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            "Press r to list pending approvals.",
            Style::default().fg(theme.dim),
        ),
        Line::styled(
            "Use y/n from the approval list once IDs are visible.",
            Style::default().fg(theme.dim),
        ),
    ]
}

fn command_backed_lines(command: &str, hint: &str, theme: &Theme) -> Vec<Line<'static>> {
    vec![
        Line::styled(hint.to_string(), Style::default().fg(theme.fg)),
        Line::styled(
            format!("Backing command: {command}"),
            Style::default().fg(theme.dim),
        ),
        Line::styled(
            "Output appears in the output view.",
            Style::default().fg(theme.dim),
        ),
    ]
}

fn rfc_lines(model: &CockpitModel<'_>, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        "PROJECT       STATUS      RFC",
        Style::default().fg(theme.dim).add_modifier(Modifier::BOLD),
    )];
    let filter = model.app.filter.to_ascii_lowercase();
    for project in &model.workspace.projects {
        for rfc in &project.rfcs {
            if !filter.is_empty()
                && !rfc.slug.to_ascii_lowercase().contains(&filter)
                && !rfc.title.to_ascii_lowercase().contains(&filter)
            {
                continue;
            }
            lines.push(Line::raw(format!(
                "{:<13} {:<11} {}",
                truncate(&project.name, 13),
                truncate(&rfc.status, 11),
                truncate(&format!("{} {}", rfc.slug, rfc.title), 48)
            )));
        }
    }
    lines
}

fn project_lines(model: &CockpitModel<'_>, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        "NAME          RFCS  ISSUES  AGENTS",
        Style::default().fg(theme.dim).add_modifier(Modifier::BOLD),
    )];
    let filter = model.app.filter.to_ascii_lowercase();
    for (idx, project) in model
        .workspace
        .projects
        .iter()
        .filter(|p| filter.is_empty() || p.name.to_ascii_lowercase().contains(&filter))
        .enumerate()
    {
        lines.push(row(
            idx == model.app.selected,
            format!(
                "{:<13} {:<5} {:<7} {}",
                truncate(&project.name, 13),
                project.rfcs.len(),
                project.issues.len(),
                truncate(&project.assigned_agents.join(","), 28)
            ),
            theme,
        ));
    }
    lines
}

fn team_lines(model: &CockpitModel<'_>, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        "IDENTIFIER    MEMBERS  NAME",
        Style::default().fg(theme.dim).add_modifier(Modifier::BOLD),
    )];
    let filter = model.app.filter.to_ascii_lowercase();
    for (idx, team) in model
        .team_snapshot
        .teams
        .iter()
        .filter(|t| {
            filter.is_empty()
                || t.identifier.to_ascii_lowercase().contains(&filter)
                || t.name.to_ascii_lowercase().contains(&filter)
        })
        .enumerate()
    {
        let members = model
            .team_snapshot
            .team_members
            .iter()
            .filter(|m| m.team_id == team.id)
            .count();
        lines.push(row(
            idx == model.app.selected,
            format!(
                "{:<13} {:<7} {}",
                truncate(&team.identifier, 13),
                members,
                truncate(&team.name, 40)
            ),
            theme,
        ));
    }
    lines
}

fn help_lines(theme: &Theme) -> Vec<Line<'static>> {
    [
        "1-9 switch views",
        "Tab / Shift-Tab cycle views",
        "j/k or arrows select",
        "/ filter resources",
        ": run any Orkia command",
        "Enter open/close detail",
        "a attach selected daemon job/stage",
        "t tell selected daemon stage",
        "s stop selected daemon job",
        "K kill selected daemon job/stage",
        "w wait selected daemon job",
        "i inspect selected daemon job",
        "l logs for selected daemon job",
        "o open first source ref on selected output card",
        "g garbage collect terminal daemon caches",
        "PageUp/PageDown scroll output or logs panel",
        "q or Ctrl-D quit TUI",
        "r run backing builtin for this view",
        "Esc closes mode/detail",
    ]
    .into_iter()
    .map(|s| Line::styled(s.to_string(), Style::default().fg(theme.fg)))
    .collect()
}

fn selected_agent_detail(model: &CockpitModel<'_>, theme: &Theme) -> Vec<Line<'static>> {
    let Some(agent) = visible_agents(model.agents, &model.app.filter)
        .get(model.app.selected)
        .copied()
    else {
        return vec![Line::styled(
            "No agent selected.",
            Style::default().fg(theme.dim),
        )];
    };
    vec![
        kv("name", &agent.name, theme),
        kv("status", agent_status_label(&agent.status), theme),
        kv("model", &agent.model, theme),
        kv("archetype", &agent.archetype, theme),
        kv("command", &agent.command, theme),
        kv("projects", &agent.assigned_projects.join(", "), theme),
        kv("dir", &agent.dir.display().to_string(), theme),
    ]
}

fn selected_project_detail(model: &CockpitModel<'_>, theme: &Theme) -> Vec<Line<'static>> {
    let filter = model.app.filter.to_ascii_lowercase();
    let Some(project) = model
        .workspace
        .projects
        .iter()
        .filter(|p| filter.is_empty() || p.name.to_ascii_lowercase().contains(&filter))
        .nth(model.app.selected)
    else {
        return vec![Line::styled(
            "No project selected.",
            Style::default().fg(theme.dim),
        )];
    };
    vec![
        kv("name", &project.name, theme),
        kv(
            "description",
            project.description.as_deref().unwrap_or("-"),
            theme,
        ),
        kv("rfcs", &project.rfcs.len().to_string(), theme),
        kv("issues", &project.issues.len().to_string(), theme),
        kv("agents", &project.assigned_agents.join(", "), theme),
        kv("path", &project.path.display().to_string(), theme),
    ]
}

fn selected_team_detail(model: &CockpitModel<'_>, theme: &Theme) -> Vec<Line<'static>> {
    let filter = model.app.filter.to_ascii_lowercase();
    let Some(team) = model
        .team_snapshot
        .teams
        .iter()
        .filter(|t| {
            filter.is_empty()
                || t.identifier.to_ascii_lowercase().contains(&filter)
                || t.name.to_ascii_lowercase().contains(&filter)
        })
        .nth(model.app.selected)
    else {
        return vec![Line::styled(
            "No team selected.",
            Style::default().fg(theme.dim),
        )];
    };
    let members = model
        .team_snapshot
        .team_members
        .iter()
        .filter(|m| m.team_id == team.id)
        .count();
    vec![
        kv("identifier", &team.identifier, theme),
        kv("name", &team.name, theme),
        kv("members", &members.to_string(), theme),
        kv("owner", &team.owner_account_id.to_string(), theme),
        kv("color", team.color.as_deref().unwrap_or("-"), theme),
    ]
}

fn view_count(view: View, model: &CockpitModel<'_>) -> usize {
    match view {
        View::Agents => visible_agents(model.agents, &model.app.filter).len(),
        View::Jobs => {
            crate::daemon::visible_daemon_rows(model.daemon_jobs, &model.app.filter).len()
        }
        View::Approvals => model.pending_approvals,
        View::Journal => 0,
        View::Seal => 1,
        View::Rfcs => model.workspace.projects.iter().map(|p| p.rfcs.len()).sum(),
        View::Projects => model.workspace.projects.len(),
        View::Teams => model.team_snapshot.teams.len(),
        View::Output => model.cards.len(),
        View::Help => 0,
    }
}

fn row(selected: bool, text: String, theme: &Theme) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    let style = if selected {
        Style::default()
            .fg(theme.fg)
            .bg(theme.bg_selected)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.fg)
    };
    Line::styled(format!("{marker}{text}"), style)
}

fn kv(key: &str, value: &str, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<12}"), Style::default().fg(theme.dim)),
        Span::styled(value.to_string(), Style::default().fg(theme.fg)),
    ])
}

fn agent_status_label(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Idle => "idle",
        AgentStatus::Working => "working",
        AgentStatus::Waiting => "waiting",
        AgentStatus::Error => "error",
    }
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

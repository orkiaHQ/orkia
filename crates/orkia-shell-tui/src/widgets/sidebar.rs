// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

use crate::theme::Theme;
use orkia_shell_types::{AgentInfo, AgentStatus, JobInfo, JobState, Workspace};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

pub fn render_sidebar(
    f: &mut Frame<'_>,
    area: Rect,
    agents: &[AgentInfo],
    jobs: &[JobInfo],
    workspace: &Workspace,
    pending_approvals: usize,
    theme: &Theme,
) {
    // Three sections: agents (size = N+2), jobs (size = max(N,1)+2), projects (fill)
    let agents_h = (agents.len().min(8) as u16).max(1) + 2;
    let jobs_h = (jobs.len().min(6) as u16).max(1) + 2;

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(agents_h),
            Constraint::Length(jobs_h),
            Constraint::Min(3),
        ])
        .split(area);

    render_agents(f, sections[0], agents, theme);
    render_jobs(f, sections[1], jobs, pending_approvals, theme);
    render_projects(f, sections[2], workspace, theme);
}

fn block_with_title(theme: &Theme, title: String) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ))
}

fn render_agents(f: &mut Frame<'_>, area: Rect, agents: &[AgentInfo], theme: &Theme) {
    let block = block_with_title(theme, " agents ".into());
    let inner = block.inner(area);
    f.render_widget(block, area);

    if agents.is_empty() {
        let p = Paragraph::new(Line::styled(
            "(none configured)",
            Style::default().fg(theme.dim),
        ));
        f.render_widget(p, inner);
        return;
    }

    let lines: Vec<Line<'_>> = agents
        .iter()
        .take(8)
        .map(|a| {
            let (dot, dot_color) = match a.status {
                AgentStatus::Idle => ("○", theme.dim),
                AgentStatus::Working => ("●", theme.green),
                AgentStatus::Waiting => ("◐", theme.yellow),
                AgentStatus::Error => ("✕", theme.red),
            };
            Line::from(vec![
                Span::styled(dot, Style::default().fg(dot_color)),
                Span::raw(" "),
                Span::styled(
                    truncate(&a.name, 8),
                    Style::default().fg(theme.agent_color(&a.name)),
                ),
                Span::raw(" "),
                // archetype only; effective trust is a per-capability view.
                Span::styled(truncate(&a.archetype, 8), Style::default().fg(theme.dim)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_jobs(
    f: &mut Frame<'_>,
    area: Rect,
    jobs: &[JobInfo],
    pending_approvals: usize,
    theme: &Theme,
) {
    let title = if pending_approvals > 0 {
        format!(" jobs · {pending_approvals} approval ")
    } else {
        " jobs ".to_string()
    };
    let block = block_with_title(theme, title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if jobs.is_empty() {
        let p = Paragraph::new(Line::styled("(none)", Style::default().fg(theme.dim)));
        f.render_widget(p, inner);
        return;
    }

    let lines: Vec<Line<'_>> = jobs
        .iter()
        .take(6)
        .map(|j| {
            let state_color = match &j.state {
                JobState::Foreground => theme.accent,
                JobState::Running => theme.green,
                JobState::Stopped => theme.yellow,
                JobState::Done { exit_code: 0 } => theme.dim,
                JobState::Done { .. } | JobState::Failed { .. } => theme.red,
            };
            Line::from(vec![
                Span::styled(format!("{:<2}", j.id.0), Style::default().fg(theme.accent)),
                Span::raw(" "),
                Span::styled(truncate(&j.label, 12), Style::default().fg(theme.fg)),
                Span::raw(" "),
                Span::styled(format!("{}", j.state), Style::default().fg(state_color)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_projects(f: &mut Frame<'_>, area: Rect, workspace: &Workspace, theme: &Theme) {
    let block = block_with_title(theme, " projects ".into());
    let inner = block.inner(area);
    f.render_widget(block, area);

    if workspace.projects.is_empty() {
        let p = Paragraph::new(vec![
            Line::styled("(no projects)", Style::default().fg(theme.dim)),
            Line::styled(
                "orkia project create <name>",
                Style::default().fg(theme.dim),
            ),
        ]);
        f.render_widget(p, inner);
        return;
    }

    let mut lines: Vec<Line<'_>> = Vec::new();
    let inner_width = inner.width as usize;
    for p in &workspace.projects {
        lines.push(Line::styled(
            truncate(&p.name, inner_width),
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        ));
        let open_issues = p.issues.iter().filter(|i| i.status != "done").count();
        lines.push(Line::styled(
            format!("  {} rfc · {} issue", p.rfcs.len(), open_issues),
            Style::default().fg(theme.dim),
        ));

        let active: Vec<&str> = p
            .rfcs
            .iter()
            .filter(|r| r.status == "active")
            .map(|r| r.slug.as_str())
            .collect();
        // Reserve room for the dot, space, and " (active)" suffix.
        let slug_max = inner_width.saturating_sub(11);
        for slug in active.iter().take(3) {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("●", Style::default().fg(theme.green)),
                Span::raw(" "),
                Span::styled(truncate(slug, slug_max), Style::default().fg(theme.fg)),
                Span::styled(" (active)", Style::default().fg(theme.dim)),
            ]));
        }
        if active.len() > 3 {
            lines.push(Line::styled(
                format!("  (+{} more)", active.len() - 3),
                Style::default().fg(theme.dim),
            ));
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let end: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{end}…")
    }
}

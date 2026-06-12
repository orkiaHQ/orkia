// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use crate::app::TuiApp;
use crate::daemon::{DaemonJobRow, DaemonRowRef, selected_row, visible_daemon_rows};
use crate::theme::Theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

pub fn daemon_job_lines(app: &TuiApp, jobs: &[DaemonJobRow], theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        "TARGET     STATE       PID      CPU    MEM    RUNTIME  ATTACH  NAME",
        Style::default().fg(theme.dim).add_modifier(Modifier::BOLD),
    )];
    for (idx, row_ref) in visible_daemon_rows(jobs, &app.filter).iter().enumerate() {
        lines.push(row(
            idx == app.selected,
            format_row(jobs, *row_ref),
            row_state(jobs, *row_ref),
            theme,
        ));
    }
    lines
}

pub fn selected_daemon_detail(
    app: &TuiApp,
    jobs: &[DaemonJobRow],
    theme: &Theme,
) -> Vec<Line<'static>> {
    let Some(row_ref) = selected_row(jobs, &app.filter, app.selected) else {
        return vec![Line::styled(
            "No daemon job selected.",
            Style::default().fg(theme.dim),
        )];
    };
    match row_ref {
        DaemonRowRef::Job { job_index } => job_detail(&jobs[job_index], theme),
        DaemonRowRef::Stage {
            job_index,
            stage_index,
        } => stage_detail(&jobs[job_index], stage_index, theme),
    }
}

fn format_row(jobs: &[DaemonJobRow], row_ref: DaemonRowRef) -> String {
    match row_ref {
        DaemonRowRef::Job { job_index } => {
            let job = &jobs[job_index];
            format!(
                "{:<10} {:<11} {:<8} {:<6} {:<6} {:<8} {:<7} {}",
                job.id,
                truncate(&job.state, 11),
                fmt_pid(job.pid),
                job.cpu_percent.as_deref().unwrap_or("-"),
                job.mem_percent.as_deref().unwrap_or("-"),
                format_secs(job.runtime_secs),
                yes_no(job.attachable),
                truncate(&job.label, 36)
            )
        }
        DaemonRowRef::Stage {
            job_index,
            stage_index,
        } => {
            let job = &jobs[job_index];
            let stage = &job.stages[stage_index];
            let target = if stage.id > 0 {
                format!("{}:{}", job.id, stage.id)
            } else {
                format!("{}:{}", job.id, stage.target)
            };
            format!(
                "{:<10} {:<11} {:<8} {:<6} {:<6} {:<8} {:<7} stage {}",
                target,
                truncate(&stage.state, 11),
                fmt_pid(stage.pid),
                stage.cpu_percent.as_deref().unwrap_or("-"),
                stage.mem_percent.as_deref().unwrap_or("-"),
                format_secs(stage.runtime_secs),
                yes_no(stage.attachable),
                truncate(&stage.target, 28)
            )
        }
    }
}

fn job_detail(job: &DaemonJobRow, theme: &Theme) -> Vec<Line<'static>> {
    vec![
        kv("target", &job.id.to_string(), theme),
        kv("agent", &job.agent, theme),
        kv("state", &job.state, theme),
        kv("pid", &fmt_pid(job.pid), theme),
        kv("runtime", &format_secs(job.runtime_secs), theme),
        kv("attachable", yes_no(job.attachable), theme),
        kv("exit_code", &fmt_i32(job.exit_code), theme),
        kv(
            "lost_reason",
            job.lost_reason.as_deref().unwrap_or("-"),
            theme,
        ),
        kv("seal_path", job.seal_path.as_deref().unwrap_or("-"), theme),
        kv(
            "pty_owner",
            &job.pty_owner_pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_string()),
            theme,
        ),
        kv(
            "socket",
            job.control_socket.as_deref().unwrap_or("-"),
            theme,
        ),
        kv("cmd", &job.label, theme),
    ]
}

fn stage_detail(job: &DaemonJobRow, stage_index: usize, theme: &Theme) -> Vec<Line<'static>> {
    let stage = &job.stages[stage_index];
    let target = if stage.id > 0 {
        format!("{}:{}", job.id, stage.id)
    } else {
        format!("{}:{}", job.id, stage.target)
    };
    vec![
        kv("target", &target, theme),
        kv("alias", &stage.target, theme),
        kv("job", &job.id.to_string(), theme),
        kv("state", &stage.state, theme),
        kv("pid", &fmt_pid(stage.pid), theme),
        kv("runtime", &format_secs(stage.runtime_secs), theme),
        kv("attachable", yes_no(stage.attachable), theme),
        kv("exit_code", &fmt_i32(stage.exit_code), theme),
        kv(
            "lost_reason",
            stage.lost_reason.as_deref().unwrap_or("-"),
            theme,
        ),
    ]
}

fn row_state(jobs: &[DaemonJobRow], row_ref: DaemonRowRef) -> &str {
    match row_ref {
        DaemonRowRef::Job { job_index } => jobs[job_index].state.as_str(),
        DaemonRowRef::Stage {
            job_index,
            stage_index,
        } => jobs[job_index].stages[stage_index].state.as_str(),
    }
}

fn row(selected: bool, text: String, state: &str, theme: &Theme) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    let fg = state_color(state, theme);
    let style = if selected {
        Style::default()
            .fg(fg)
            .bg(theme.bg_selected)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(fg)
    };
    Line::styled(format!("{marker}{text}"), style)
}

fn state_color(state: &str, theme: &Theme) -> Color {
    match state {
        "detached" | "running" | "foreground" => theme.green,
        "stopped" | "unknown" => theme.yellow,
        "done" => theme.dim,
        "pid_dead" | "lost_pty" | "control_unavailable" | "failed" | "error" => theme.red,
        _ => theme.fg,
    }
}

fn kv(key: &str, value: &str, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<12}"), Style::default().fg(theme.dim)),
        Span::styled(value.to_string(), Style::default().fg(theme.fg)),
    ])
}

fn fmt_pid(pid: Option<u32>) -> String {
    pid.map(|p| p.to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn fmt_i32(value: Option<i32>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn format_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
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

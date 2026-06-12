// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use crate::daemon::{DaemonAction, DaemonJobRow, command_for, selected_row, visible_daemon_rows};
use orkia_shell_types::{AgentInfo, JobInfo, Workspace};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Agents,
    Jobs,
    Approvals,
    Journal,
    Seal,
    Rfcs,
    Projects,
    Teams,
    Output,
    Help,
}

impl View {
    pub const ALL: [Self; 10] = [
        Self::Agents,
        Self::Jobs,
        Self::Approvals,
        Self::Journal,
        Self::Seal,
        Self::Rfcs,
        Self::Projects,
        Self::Teams,
        Self::Output,
        Self::Help,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Agents => "agents",
            Self::Jobs => "jobs",
            Self::Approvals => "approvals",
            Self::Journal => "journal",
            Self::Seal => "seal",
            Self::Rfcs => "rfcs",
            Self::Projects => "projects",
            Self::Teams => "teams",
            Self::Output => "output",
            Self::Help => "help",
        }
    }

    pub fn command(self) -> &'static str {
        match self {
            Self::Agents => "agent",
            Self::Jobs => "ps",
            Self::Approvals => "approve",
            Self::Journal => "journal",
            Self::Seal => "seal --verify",
            Self::Rfcs => "rfc ls",
            Self::Projects => "project ls",
            Self::Teams => "team ls",
            Self::Output => "",
            Self::Help => "help",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Filter,
    Command,
    Tell,
    ConfirmKill,
}

impl InputMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Filter => "filter",
            Self::Command => "command",
            Self::Tell => "tell",
            Self::ConfirmKill => "confirm",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TuiApp {
    pub view: View,
    pub mode: InputMode,
    pub filter: String,
    pub buffer: String,
    pub selected: usize,
    pub source_selected: usize,
    pub detail_open: bool,
    pub status: String,
}

impl Default for TuiApp {
    fn default() -> Self {
        Self {
            view: View::Agents,
            mode: InputMode::Normal,
            filter: String::new(),
            buffer: String::new(),
            selected: 0,
            source_selected: 0,
            detail_open: false,
            status: "Press ? for keys, : for commands, / to filter".into(),
        }
    }
}

impl TuiApp {
    pub fn set_view(&mut self, view: View) {
        self.view = view;
        self.selected = 0;
        self.source_selected = 0;
        self.detail_open = false;
        self.status = format!("view: {}", view.label());
    }

    pub fn next_view(&mut self) {
        let pos = View::ALL.iter().position(|v| *v == self.view).unwrap_or(0);
        let next = View::ALL[(pos + 1) % View::ALL.len()];
        self.set_view(next);
    }

    pub fn prev_view(&mut self) {
        let pos = View::ALL.iter().position(|v| *v == self.view).unwrap_or(0);
        let next = if pos == 0 {
            View::ALL[View::ALL.len() - 1]
        } else {
            View::ALL[pos - 1]
        };
        self.set_view(next);
    }

    pub fn begin_filter(&mut self) {
        self.mode = InputMode::Filter;
        self.buffer = self.filter.clone();
        self.status = "filter resources".into();
    }

    pub fn begin_command(&mut self) {
        self.mode = InputMode::Command;
        self.buffer.clear();
        self.status = "run Orkia command".into();
    }

    pub fn begin_tell(&mut self) {
        self.mode = InputMode::Tell;
        self.buffer.clear();
        self.status = "tell selected agent/job".into();
    }

    pub fn begin_kill_confirm(&mut self) {
        self.mode = InputMode::ConfirmKill;
        self.buffer.clear();
        self.status = "confirm kill with y, cancel with Esc".into();
    }

    pub fn cancel_mode(&mut self) {
        self.mode = InputMode::Normal;
        self.buffer.clear();
        self.status.clear();
    }

    pub fn push_char(&mut self, c: char) {
        self.buffer.push(c);
        if self.mode == InputMode::Filter {
            self.filter = self.buffer.clone();
            self.selected = 0;
            self.source_selected = 0;
        }
    }

    pub fn backspace(&mut self) {
        self.buffer.pop();
        if self.mode == InputMode::Filter {
            self.filter = self.buffer.clone();
            self.selected = 0;
            self.source_selected = 0;
        }
    }

    pub fn clear_filter(&mut self) {
        self.filter.clear();
        if self.mode == InputMode::Filter {
            self.buffer.clear();
        }
        self.selected = 0;
        self.source_selected = 0;
        self.status = "filter cleared".into();
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self, visible_len: usize) {
        if self.selected + 1 < visible_len {
            self.selected += 1;
        }
    }

    pub fn move_source_up(&mut self) {
        self.source_selected = self.source_selected.saturating_sub(1);
    }

    pub fn move_source_down(&mut self, visible_len: usize) {
        if self.source_selected + 1 < visible_len {
            self.source_selected += 1;
        }
    }

    pub fn clamp_source_selection(&mut self, visible_len: usize) {
        if visible_len == 0 {
            self.source_selected = 0;
        } else if self.source_selected >= visible_len {
            self.source_selected = visible_len - 1;
        }
    }

    pub fn clamp_selection(&mut self, visible_len: usize) {
        if visible_len == 0 {
            self.selected = 0;
        } else if self.selected >= visible_len {
            self.selected = visible_len - 1;
        }
    }

    pub fn selected_job<'a>(&self, jobs: &'a [JobInfo]) -> Option<&'a JobInfo> {
        visible_jobs(jobs, &self.filter).get(self.selected).copied()
    }

    pub fn selected_agent<'a>(&self, agents: &'a [AgentInfo]) -> Option<&'a AgentInfo> {
        visible_agents(agents, &self.filter)
            .get(self.selected)
            .copied()
    }

    pub fn selected_target(&self, agents: &[AgentInfo], jobs: &[JobInfo]) -> Option<String> {
        match self.view {
            View::Agents => self.selected_agent(agents).map(|a| a.name.clone()),
            View::Jobs => self.selected_job(jobs).map(|j| j.id.to_string()),
            _ => None,
        }
    }

    pub fn visible_len(
        &self,
        agents: &[AgentInfo],
        _jobs: &[JobInfo],
        daemon_jobs: &[DaemonJobRow],
        workspace: &Workspace,
    ) -> usize {
        match self.view {
            View::Agents => visible_agents(agents, &self.filter).len(),
            View::Jobs => visible_daemon_rows(daemon_jobs, &self.filter).len(),
            View::Projects => visible_projects(workspace, &self.filter).len(),
            View::Rfcs => workspace.projects.iter().map(|p| p.rfcs.len()).sum(),
            _ => 1,
        }
    }

    pub fn selected_daemon_command(
        &self,
        daemon_jobs: &[DaemonJobRow],
        action: DaemonAction,
        body: Option<&str>,
    ) -> Option<String> {
        if action == DaemonAction::Gc {
            return Some("ps --gc --json".to_string());
        }
        if self.view != View::Jobs {
            return None;
        }
        let row = selected_row(daemon_jobs, &self.filter, self.selected)?;
        command_for(daemon_jobs, row, action, body)
    }

    pub fn selected_attach_command(&self, daemon_jobs: &[DaemonJobRow]) -> Option<String> {
        self.selected_daemon_command(daemon_jobs, DaemonAction::Attach, None)
    }

    pub fn selected_stop_command(&self, daemon_jobs: &[DaemonJobRow]) -> Option<String> {
        self.selected_daemon_command(daemon_jobs, DaemonAction::Stop, None)
    }

    pub fn selected_kill_command(&self, daemon_jobs: &[DaemonJobRow]) -> Option<String> {
        self.selected_daemon_command(daemon_jobs, DaemonAction::Kill, None)
    }

    pub fn selected_tell_command(
        &self,
        daemon_jobs: &[DaemonJobRow],
        body: &str,
    ) -> Option<String> {
        self.selected_daemon_command(daemon_jobs, DaemonAction::Tell, Some(body))
    }

    pub fn selected_wait_command(&self, daemon_jobs: &[DaemonJobRow]) -> Option<String> {
        self.selected_daemon_command(daemon_jobs, DaemonAction::Wait, None)
    }

    pub fn selected_inspect_command(&self, daemon_jobs: &[DaemonJobRow]) -> Option<String> {
        self.selected_daemon_command(daemon_jobs, DaemonAction::Inspect, None)
    }

    pub fn selected_logs_command(&self, daemon_jobs: &[DaemonJobRow]) -> Option<String> {
        self.selected_daemon_command(daemon_jobs, DaemonAction::Logs, None)
    }

    pub fn selected_attach_command_legacy(
        &self,
        agents: &[AgentInfo],
        jobs: &[JobInfo],
    ) -> Option<String> {
        match self.view {
            View::Agents => {
                let _ = agents;
                None
            }
            View::Jobs => self.selected_job(jobs).map(|j| format!("attach %{}", j.id)),
            _ => None,
        }
    }
}

pub fn visible_agents<'a>(agents: &'a [AgentInfo], filter: &str) -> Vec<&'a AgentInfo> {
    let f = filter.trim().to_ascii_lowercase();
    agents
        .iter()
        .filter(|a| {
            f.is_empty()
                || a.name.to_ascii_lowercase().contains(&f)
                || a.archetype.to_ascii_lowercase().contains(&f)
                || a.model.to_ascii_lowercase().contains(&f)
        })
        .collect()
}

pub fn visible_jobs<'a>(jobs: &'a [JobInfo], filter: &str) -> Vec<&'a JobInfo> {
    let f = filter.trim().to_ascii_lowercase();
    jobs.iter()
        .filter(|j| {
            f.is_empty()
                || j.label.to_ascii_lowercase().contains(&f)
                || j.id.to_string().contains(&f)
                || j.state.to_string().to_ascii_lowercase().contains(&f)
                || j.kind.to_string().to_ascii_lowercase().contains(&f)
        })
        .collect()
}

pub fn visible_projects<'a>(workspace: &'a Workspace, filter: &str) -> Vec<&'a str> {
    let f = filter.trim().to_ascii_lowercase();
    workspace
        .projects
        .iter()
        .filter(|p| f.is_empty() || p.name.to_ascii_lowercase().contains(&f))
        .map(|p| p.name.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_shell_types::{AgentStatus, JobId, JobKind, JobState, Project};
    use std::path::PathBuf;
    use std::time::Duration;
    use uuid::Uuid;

    fn agent(name: &str, archetype: &str) -> AgentInfo {
        AgentInfo {
            id: Uuid::nil(),
            name: name.into(),
            archetype: archetype.into(),
            status: AgentStatus::Idle,
            model: "codex".into(),
            dir: PathBuf::new(),
            description: None,
            command: "codex".into(),
            args: Vec::new(),
            assigned_projects: Vec::new(),
            max_context_tokens: 0,
        }
    }

    fn job(id: u32, label: &str) -> JobInfo {
        JobInfo {
            id: JobId(id),
            kind: JobKind::Shell { cmd: label.into() },
            state: JobState::Running,
            label: label.into(),
            pid: Some(1000 + id),
            runtime: Duration::from_secs(4),
            sink: None,
        }
    }

    fn workspace() -> Workspace {
        Workspace {
            root: PathBuf::new(),
            projects: vec![Project {
                name: "core".into(),
                description: None,
                assigned_agents: Vec::new(),
                rfcs: Vec::new(),
                issues: Vec::new(),
                path: PathBuf::from("/tmp/core"),
                scope: None,
            }],
        }
    }

    #[test]
    fn filter_matches_agents_by_name_or_archetype() {
        let agents = vec![agent("faye", "backend"), agent("sage", "review")];
        assert_eq!(visible_agents(&agents, "backend").len(), 1);
        assert_eq!(visible_agents(&agents, "SAGE")[0].name, "sage");
    }

    #[test]
    fn selection_clamps_to_visible_rows() {
        let mut app = TuiApp {
            selected: 10,
            ..TuiApp::default()
        };
        app.clamp_selection(2);
        assert_eq!(app.selected, 1);
        app.clamp_selection(0);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn view_switch_resets_selection_and_detail() {
        let mut app = TuiApp {
            selected: 4,
            source_selected: 2,
            detail_open: true,
            ..TuiApp::default()
        };
        app.set_view(View::Jobs);
        assert_eq!(app.view, View::Jobs);
        assert_eq!(app.selected, 0);
        assert_eq!(app.source_selected, 0);
        assert!(!app.detail_open);
    }

    #[test]
    fn source_selection_moves_and_clamps() {
        let mut app = TuiApp::default();
        app.move_source_down(3);
        app.move_source_down(3);
        app.move_source_down(3);
        assert_eq!(app.source_selected, 2);
        app.move_source_up();
        assert_eq!(app.source_selected, 1);
        app.clamp_source_selection(1);
        assert_eq!(app.source_selected, 0);
        app.move_source_down(0);
        app.clamp_source_selection(0);
        assert_eq!(app.source_selected, 0);
    }

    #[test]
    fn selected_commands_use_current_resource() {
        let agents = vec![agent("faye", "backend")];
        let jobs = vec![job(7, "make test")];
        let mut app = TuiApp::default();

        assert_eq!(
            app.selected_attach_command_legacy(&agents, &jobs)
                .as_deref(),
            None
        );

        app.set_view(View::Jobs);
        assert_eq!(
            app.selected_attach_command_legacy(&agents, &jobs)
                .as_deref(),
            Some("attach %7")
        );
    }

    #[test]
    fn visible_len_uses_active_view() {
        let agents = vec![agent("faye", "backend")];
        let jobs = vec![job(1, "build"), job(2, "test")];
        let ws = workspace();
        let mut app = TuiApp::default();
        let daemon_jobs = Vec::new();
        assert_eq!(app.visible_len(&agents, &jobs, &daemon_jobs, &ws), 1);
        app.set_view(View::Jobs);
        assert_eq!(app.visible_len(&agents, &jobs, &daemon_jobs, &ws), 0);
        app.set_view(View::Projects);
        assert_eq!(app.visible_len(&agents, &jobs, &daemon_jobs, &ws), 1);
    }
}

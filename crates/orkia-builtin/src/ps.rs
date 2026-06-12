// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! renderers, daemon-state mapping, and metrics sampling. The REPL builtin,
//! the CLI `orkia ps` subcommand, and the daemon-jobs bridge fold are thin
//! frontends that only GATHER rows — none of them format.

use std::collections::HashMap;

use orkia_shell_types::{
    AgentInfo, BlockContent, DaemonJobView, JobInfo, JobKind, JobOwner, JobState, PsFlags,
    render_job_id,
};
use serde_json::json;

const SYSTEM_PROCESS_LIMIT: usize = 15;

/// Render-oriented model. `JobInfo` stays the control-flow type — this is
/// what the renderers consume, built by the frontends.
pub struct PsModel {
    /// `Some` includes the `agents` key in JSON (REPL frontend); `None`
    /// omits it (CLI frontend has no agent roster).
    pub agents: Option<Vec<PsAgent>>,
    pub jobs: Vec<PsRow>,
}

pub struct PsAgent {
    pub name: String,
    pub model: String,
}

impl PsAgent {
    pub fn from_info(a: &AgentInfo) -> Self {
        Self {
            name: a.name.clone(),
            model: a.model.clone(),
        }
    }
}

/// One job row, unifying the `JobInfo`-derived (REPL-local) and
/// daemon-derived shapes. `state` is the display vocabulary (a local job's
/// `JobState` rendering, or the daemon's raw state string); `state_typed`
/// is the single collapsed mapping (`job_state`).
pub struct PsRow {
    pub id: u32,
    /// REPL-local (`%N`) vs daemon-owned (`[N]`) — set from which constructor
    /// built the row (`from_job_info` vs `from_daemon_view`), driving the
    /// rendered JOB-column prefix.
    pub owner: JobOwner,
    pub agent: String,
    pub state: String,
    pub state_typed: JobState,
    pub pid: Option<u32>,
    pub label: String,
    pub runtime_secs: u64,
    pub sink: Option<String>,
    pub exit_code: Option<i32>,
    pub attachable: bool,
    pub is_app: bool,
    pub control_socket: Option<String>,
    pub pty_owner_pid: Option<u32>,
    pub lost_reason: Option<String>,
    pub seal_path: Option<String>,
    pub stages: Vec<PsStageRow>,
}

pub struct PsStageRow {
    pub id: u32,
    pub target: String,
    pub state: String,
    pub pid: Option<u32>,
    pub runtime_secs: u64,
    pub exit_code: Option<i32>,
    pub attachable: bool,
    pub lost_reason: Option<String>,
}

impl PsRow {
    /// REPL-local job (`JobController` roster).
    pub fn from_job_info(job: &JobInfo) -> Self {
        let agent = match &job.kind {
            JobKind::Agent { agent_name, .. } => agent_name.clone(),
            JobKind::Shell { .. } => "shell".into(),
            JobKind::ForgeApp { app_name } => app_name.clone(),
        };
        let exit_code = match &job.state {
            JobState::Done { exit_code } => Some(*exit_code),
            _ => None,
        };
        let attachable = !matches!(job.state, JobState::Done { .. } | JobState::Failed { .. });
        Self {
            id: job.id.0,
            owner: JobOwner::Local,
            agent,
            state: job.state.to_string(),
            state_typed: job.state.clone(),
            pid: job.pid,
            label: job.label.clone(),
            runtime_secs: job.runtime.as_secs(),
            sink: job.sink.clone(),
            exit_code,
            attachable,
            is_app: matches!(job.kind, JobKind::ForgeApp { .. }),
            control_socket: None,
            pty_owner_pid: None,
            lost_reason: None,
            seal_path: None,
            stages: Vec::new(),
        }
    }

    pub fn from_daemon_view(v: &DaemonJobView) -> Self {
        Self {
            id: v.id,
            owner: JobOwner::Daemon,
            agent: v.agent.clone(),
            state: v.state.clone(),
            state_typed: job_state(&v.state, v.exit_code),
            pid: v.pid,
            label: v.label.clone(),
            runtime_secs: v.runtime_secs,
            sink: None,
            exit_code: v.exit_code,
            attachable: !matches!(
                job_state(&v.state, v.exit_code),
                JobState::Done { .. } | JobState::Failed { .. }
            ),
            is_app: false,
            control_socket: None,
            pty_owner_pid: None,
            lost_reason: None,
            seal_path: None,
            stages: v
                .stages
                .iter()
                .map(|s| PsStageRow {
                    id: s.id,
                    target: s.target.clone(),
                    state: s.state.clone(),
                    pid: s.pid,
                    runtime_secs: s.runtime_secs,
                    exit_code: s.exit_code,
                    attachable: s.attachable,
                    lost_reason: None,
                })
                .collect(),
        }
    }
}

/// recorded exit code → typed `JobState`. Replaces the prefix-matching
/// that lived in `daemon_view_to_job_info`. `Done` carries the daemon's
/// real exit code (a recorded code of `None` on a done job means the
/// daemon observed a clean reap — 0). The collapse vocabulary (anything
pub fn job_state(raw: &str, exit_code: Option<i32>) -> JobState {
    if raw.starts_with("done") {
        JobState::Done {
            exit_code: exit_code.unwrap_or(0),
        }
    } else if raw.starts_with("fail") {
        JobState::Failed {
            reason: raw.to_string(),
        }
    } else {
        JobState::Running
    }
}

/// Single entry point for every frontend: text blocks, or one JSON block.
pub fn render(model: &PsModel, flags: &PsFlags) -> Vec<BlockContent> {
    if flags.json {
        return vec![BlockContent::Text(render_json(model, flags))];
    }
    render_text(model, flags)
}

fn render_text(model: &PsModel, flags: &PsFlags) -> Vec<BlockContent> {
    let mut blocks = Vec::new();
    let system_rows = load_system_processes();
    let metrics = metrics_by_pid(&system_rows);

    if flags.show_agents {
        let (app_jobs, non_app_jobs): (Vec<&PsRow>, Vec<&PsRow>) =
            model.jobs.iter().partition(|j| j.is_app);
        push_agents_section(&mut blocks, &non_app_jobs, &metrics);
        if !app_jobs.is_empty() {
            blocks.push(BlockContent::SystemInfo(String::new()));
            push_apps_section(&mut blocks, &app_jobs);
        }
    }

    if flags.show_system {
        if !blocks.is_empty() {
            blocks.push(BlockContent::SystemInfo(String::new()));
        }
        push_system_section(&mut blocks, flags.full, system_rows);
    }

    blocks
}

fn push_agents_section(
    blocks: &mut Vec<BlockContent>,
    jobs: &[&PsRow],
    metrics: &HashMap<u32, ProcessMetrics>,
) {
    if jobs.is_empty() {
        blocks.push(BlockContent::SystemInfo(" AGENTS — none running".into()));
        return;
    }
    blocks.push(BlockContent::SystemInfo(" AGENTS".into()));
    // corresponded to no decision the system makes. Effective per-(project ×
    // capability) trust lives in the `trust` builtin's session-marked view.
    // `AGENTS` section header was redundant).
    blocks.push(BlockContent::SystemInfo(
        " JOB  AGENT      STATUS     PID      CPU    MEM    CMD                  RUNTIME    SINK"
            .into(),
    ));
    for job in jobs {
        blocks.push(BlockContent::Text(format_job_line(job, metrics)));
        for stage in &job.stages {
            blocks.push(BlockContent::Text(format_stage_line(
                job.id, stage, metrics,
            )));
        }
    }
}

fn push_apps_section(blocks: &mut Vec<BlockContent>, jobs: &[&PsRow]) {
    blocks.push(BlockContent::SystemInfo(" APPS".into()));
    blocks.push(BlockContent::SystemInfo(
        " JOB  APP              STATUS     PID      RUNTIME".into(),
    ));
    for job in jobs {
        let pid = job.pid.map_or_else(|| "-".into(), |p| p.to_string());
        blocks.push(BlockContent::Text(format!(
            " {:<4} {:<16} {:<10} {:<8} {}",
            render_job_id(job.owner, job.id, None),
            truncate(&job.agent, 16),
            job.state,
            pid,
            fmt_runtime(job.runtime_secs),
        )));
    }
}

fn format_job_line(job: &PsRow, metrics: &HashMap<u32, ProcessMetrics>) -> String {
    let cmd = truncate(&job.label, 20);
    let pid = job.pid.map_or_else(|| "-".into(), |p| p.to_string());
    let (cpu, mem) = cpu_mem(job.pid, metrics);
    let sink = match &job.sink {
        Some(s) => format!("→ {}", truncate(s, 24)),
        None => "-".into(),
    };
    format!(
        " {:<4} {:<10} {:<10} {:<8} {:<6} {:<6} {:<20} {:<10} {}",
        render_job_id(job.owner, job.id, None),
        job.agent,
        job.state,
        pid,
        cpu,
        mem,
        cmd,
        fmt_runtime(job.runtime_secs),
        sink,
    )
}

fn format_stage_line(
    job_id: u32,
    stage: &PsStageRow,
    metrics: &HashMap<u32, ProcessMetrics>,
) -> String {
    let pid = stage.pid.map_or_else(|| "-".into(), |p| p.to_string());
    let (cpu, mem) = cpu_mem(stage.pid, metrics);
    format!(
        " {:<4} {:<10} {:<10} {:<8} {:<6} {:<6} {:<20} {:<10} -",
        // Stages are always daemon-owned → `[job:stage]`, same bracket rule.
        render_job_id(JobOwner::Daemon, job_id, Some(stage.id)),
        stage.target,
        stage.state,
        pid,
        cpu,
        mem,
        "stage",
        fmt_runtime(stage.runtime_secs),
    )
}

fn cpu_mem(pid: Option<u32>, metrics: &HashMap<u32, ProcessMetrics>) -> (&str, &str) {
    pid.and_then(|p| metrics.get(&p))
        .map(|m| (m.cpu.as_str(), m.mem.as_str()))
        .unwrap_or(("-", "-"))
}

fn fmt_runtime(secs: u64) -> String {
    format!("{}m{:02}s", secs / 60, secs % 60)
}

fn push_system_section(blocks: &mut Vec<BlockContent>, full: bool, mut rows: Vec<SysProc>) {
    blocks.push(BlockContent::SystemInfo(" PROCESSES".into()));
    blocks.push(BlockContent::SystemInfo(
        " PID      USER       CPU    MEM    CMD".into(),
    ));
    rows.sort_by(|a, b| {
        b.cpu_sort
            .partial_cmp(&a.cpu_sort)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for line in render_system_rows(rows, full) {
        blocks.push(BlockContent::Text(line));
    }
}

#[derive(Debug, Clone)]
struct SysProc {
    pid: String,
    user: String,
    cpu: String,
    mem: String,
    cmd: String,
    cpu_sort: f32,
}

#[derive(Debug, Clone)]
struct ProcessMetrics {
    cpu: String,
    mem: String,
}

/// `ps -A -o pid,user,%cpu,%mem,comm` works on both macOS and Linux. We
/// sort in Rust by %CPU desc so we don't depend on the procps-ng `--sort`
/// extension.
fn load_system_processes() -> Vec<SysProc> {
    let raw = match std::process::Command::new("ps")
        .args(["-A", "-o", "pid,user,%cpu,%mem,comm"])
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        _ => return Vec::new(),
    };

    raw.lines().skip(1).filter_map(parse_ps_row).collect()
}

fn render_system_rows(rows: Vec<SysProc>, full: bool) -> Vec<String> {
    if rows.is_empty() {
        return vec![" (ps unavailable)".into()];
    }
    let limit = if full {
        rows.len()
    } else {
        SYSTEM_PROCESS_LIMIT.min(rows.len())
    };
    rows.into_iter()
        .take(limit)
        .map(|p| {
            format!(
                " {:<8} {:<10} {:<6} {:<6} {}",
                p.pid,
                truncate(&p.user, 10),
                p.cpu,
                p.mem,
                truncate(&p.cmd, 60),
            )
        })
        .collect()
}

fn metrics_by_pid(rows: &[SysProc]) -> HashMap<u32, ProcessMetrics> {
    rows.iter()
        .filter_map(|p| {
            let pid = p.pid.parse::<u32>().ok()?;
            Some((
                pid,
                ProcessMetrics {
                    cpu: p.cpu.clone(),
                    mem: p.mem.clone(),
                },
            ))
        })
        .collect()
}

fn parse_ps_row(line: &str) -> Option<SysProc> {
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.to_string();
    let user = parts.next()?.to_string();
    let cpu = parts.next()?.to_string();
    let mem = parts.next()?.to_string();
    let cmd: String = parts.collect::<Vec<_>>().join(" ");
    if cmd.is_empty() {
        return None;
    }
    let cpu_sort = cpu.parse::<f32>().unwrap_or(0.0);
    Some(SysProc {
        pid,
        user,
        cpu,
        mem,
        cmd,
        cpu_sort,
    })
}

/// The CLI's old `daemon_jobs` top-level key is retired (pre-0.1 break).
fn render_json(model: &PsModel, flags: &PsFlags) -> String {
    let system_rows = load_system_processes();
    let metrics = metrics_by_pid(&system_rows);
    let jobs_json: Vec<_> = model
        .jobs
        .iter()
        .map(|j| {
            let proc = j.pid.and_then(|pid| metrics.get(&pid));
            json!({
                // Numeric id stays machine-parseable; `owner` carries the
                // local/daemon distinction the `%N`/`[N]` prefix encodes in text.
                "id": j.id,
                "owner": match j.owner {
                    JobOwner::Local => "local",
                    JobOwner::Daemon => "daemon",
                },
                "agent": j.agent,
                "state": j.state,
                "pid": j.pid,
                "cpu_percent": proc.map(|p| p.cpu.as_str()),
                "mem_percent": proc.map(|p| p.mem.as_str()),
                "label": j.label,
                "runtime_secs": j.runtime_secs,
                "sink": j.sink,
                "exit_code": j.exit_code,
                "attachable": j.attachable,
                "control_socket": j.control_socket,
                "pty_owner_pid": j.pty_owner_pid,
                "lost_reason": j.lost_reason,
                "seal_path": j.seal_path,
                "stages": j.stages.iter().map(|s| {
                    let proc = s.pid.and_then(|pid| metrics.get(&pid));
                    json!({
                        "id": s.id,
                        "target": s.target,
                        "state": s.state,
                        "pid": s.pid,
                        "cpu_percent": proc.map(|p| p.cpu.as_str()),
                        "mem_percent": proc.map(|p| p.mem.as_str()),
                        "runtime_secs": s.runtime_secs,
                        "exit_code": s.exit_code,
                        "attachable": s.attachable,
                        "lost_reason": s.lost_reason,
                    })
                }).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut payload = serde_json::Map::new();
    if let Some(agents) = &model.agents {
        let agents_json: Vec<_> = agents
            .iter()
            .map(|a| json!({ "name": a.name, "model": a.model }))
            .collect();
        payload.insert("agents".into(), json!(agents_json));
    }
    payload.insert("jobs".into(), json!(jobs_json));
    payload.insert("system_included".into(), json!(flags.show_system));
    serde_json::to_string_pretty(&serde_json::Value::Object(payload))
        .unwrap_or_else(|_| "{}".into())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let end: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{end}…")
    }
}

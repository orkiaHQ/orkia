// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DaemonPs {
    /// `daemon_jobs` key was retired with the ps consolidation.
    #[serde(default)]
    pub jobs: Vec<DaemonJobRow>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DaemonJobRow {
    pub id: u32,
    pub agent: String,
    pub state: String,
    pub pid: Option<u32>,
    #[serde(default)]
    pub cpu_percent: Option<String>,
    #[serde(default)]
    pub mem_percent: Option<String>,
    pub label: String,
    pub runtime_secs: u64,
    #[serde(default)]
    pub control_socket: Option<String>,
    #[serde(default)]
    pub pty_owner_pid: Option<u32>,
    #[serde(default)]
    pub lost_reason: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub seal_path: Option<String>,
    #[serde(default)]
    pub attachable: bool,
    #[serde(default)]
    pub stages: Vec<DaemonStageRow>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DaemonStageRow {
    #[serde(default)]
    pub id: u32,
    pub target: String,
    pub state: String,
    pub pid: Option<u32>,
    #[serde(default)]
    pub cpu_percent: Option<String>,
    #[serde(default)]
    pub mem_percent: Option<String>,
    pub runtime_secs: u64,
    #[serde(default)]
    pub lost_reason: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub attachable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonRowRef {
    Job {
        job_index: usize,
    },
    Stage {
        job_index: usize,
        stage_index: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonAction {
    Attach,
    Tell,
    Stop,
    Kill,
    Wait,
    Inspect,
    Logs,
    Gc,
}

pub fn parse_ps_json(raw: &str) -> Result<Vec<DaemonJobRow>, serde_json::Error> {
    serde_json::from_str::<DaemonPs>(raw).map(|ps| ps.jobs)
}

pub fn visible_daemon_rows(jobs: &[DaemonJobRow], filter: &str) -> Vec<DaemonRowRef> {
    let f = filter.trim().to_ascii_lowercase();
    let mut rows = Vec::new();
    for (job_index, job) in jobs.iter().enumerate() {
        let job_matches = f.is_empty()
            || job.id.to_string().contains(&f)
            || job.agent.to_ascii_lowercase().contains(&f)
            || job.state.to_ascii_lowercase().contains(&f)
            || job.label.to_ascii_lowercase().contains(&f)
            || job
                .lost_reason
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains(&f);
        if job_matches {
            rows.push(DaemonRowRef::Job { job_index });
        }
        for (stage_index, stage) in job.stages.iter().enumerate() {
            let stage_matches = f.is_empty()
                || job_matches
                || stage.id.to_string().contains(&f)
                || stage.target.to_ascii_lowercase().contains(&f)
                || stage.state.to_ascii_lowercase().contains(&f)
                || stage
                    .lost_reason
                    .as_deref()
                    .unwrap_or_default()
                    .to_ascii_lowercase()
                    .contains(&f);
            if stage_matches {
                rows.push(DaemonRowRef::Stage {
                    job_index,
                    stage_index,
                });
            }
        }
    }
    rows
}

pub fn selected_row(jobs: &[DaemonJobRow], filter: &str, selected: usize) -> Option<DaemonRowRef> {
    visible_daemon_rows(jobs, filter).get(selected).copied()
}

pub fn command_for(
    jobs: &[DaemonJobRow],
    row: DaemonRowRef,
    action: DaemonAction,
    body: Option<&str>,
) -> Option<String> {
    match row {
        DaemonRowRef::Job { job_index } => {
            let job = jobs.get(job_index)?;
            job_command(job, action)
        }
        DaemonRowRef::Stage {
            job_index,
            stage_index,
        } => {
            let job = jobs.get(job_index)?;
            let stage = job.stages.get(stage_index)?;
            stage_command(job, stage, action, body)
        }
    }
}

fn job_command(job: &DaemonJobRow, action: DaemonAction) -> Option<String> {
    match action {
        // job-id form the REPL gate accepts.
        DaemonAction::Attach if job.attachable => Some(format!("attach %{}", job.id)),
        DaemonAction::Stop => Some(format!("stop {}", job.id)),
        DaemonAction::Kill => Some(format!("kill {}", job.id)),
        DaemonAction::Wait => Some(format!("wait {} --timeout 30", job.id)),
        DaemonAction::Inspect => Some(format!("inspect {}", job.id)),
        DaemonAction::Logs => Some(format!("logs {} --last 100", job.id)),
        DaemonAction::Gc => Some("ps --gc --json".to_string()),
        DaemonAction::Attach | DaemonAction::Tell => None,
    }
}

fn stage_command(
    job: &DaemonJobRow,
    stage: &DaemonStageRow,
    action: DaemonAction,
    body: Option<&str>,
) -> Option<String> {
    let target = stage_target(job.id, stage);
    match action {
        // `N:@name` — prefer the agent address over the numeric stage id.
        DaemonAction::Attach if stage.attachable => {
            Some(format!("attach {}", attach_stage_target(job.id, stage)))
        }
        DaemonAction::Tell => {
            let body = body?.trim();
            if body.is_empty() {
                None
            } else {
                Some(format!("tell {target} {body}"))
            }
        }
        DaemonAction::Kill => Some(format!("kill {target}")),
        DaemonAction::Wait => Some(format!("wait {} --timeout 30", job.id)),
        DaemonAction::Inspect => Some(format!("inspect {}", job.id)),
        DaemonAction::Logs => Some(format!("logs {} --last 100", job.id)),
        DaemonAction::Gc => Some("ps --gc --json".to_string()),
        DaemonAction::Stop | DaemonAction::Attach => None,
    }
}

fn stage_target(job_id: u32, stage: &DaemonStageRow) -> String {
    if stage.id > 0 {
        format!("{job_id}:{}", stage.id)
    } else {
        format!("{job_id}:{}", stage.target)
    }
}

/// the numeric stage id when no agent address is known.
fn attach_stage_target(job_id: u32, stage: &DaemonStageRow) -> String {
    if stage.target.starts_with('@') {
        format!("{job_id}:{}", stage.target)
    } else {
        stage_target(job_id, stage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_jobs() -> Vec<DaemonJobRow> {
        parse_ps_json(
            r#"{
              "jobs": [{
                "id": 7,
                "agent": "echo",
                "state": "detached",
                "pid": 123,
                "cpu_percent": "0.0",
                "mem_percent": "0.1",
                "label": "@echo hi",
                "runtime_secs": 9,
                "control_socket": "/tmp/control.sock",
                "pty_owner_pid": 99,
                "seal_path": "agents/daemon/jobs/7/seal.jsonl",
                "attachable": true,
                "stages": [{
                  "id": 2,
                  "target": "@echo",
                  "state": "running",
                  "pid": 124,
                  "cpu_percent": "0.0",
                  "mem_percent": "0.1",
                  "runtime_secs": 8,
                  "attachable": true
                }]
              }]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn parses_ps_json_with_stage_metadata() {
        let jobs = sample_jobs();
        assert_eq!(jobs[0].id, 7);
        assert!(jobs[0].attachable);
        assert_eq!(jobs[0].stages[0].id, 2);
        assert_eq!(jobs[0].stages[0].target, "@echo");
        assert!(jobs[0].stages[0].attachable);
    }

    #[test]
    fn stage_actions_use_job_id_and_stage_id() {
        let jobs = sample_jobs();
        let row = DaemonRowRef::Stage {
            job_index: 0,
            stage_index: 0,
        };
        assert_eq!(
            command_for(&jobs, row, DaemonAction::Attach, None).as_deref(),
            // keep the numeric stage id.
            Some("attach 7:@echo")
        );
        assert_eq!(
            command_for(&jobs, row, DaemonAction::Tell, Some("hello")).as_deref(),
            Some("tell 7:2 hello")
        );
        assert_eq!(
            command_for(&jobs, row, DaemonAction::Kill, None).as_deref(),
            Some("kill 7:2")
        );
    }

    #[test]
    fn terminal_jobs_are_not_attachable() {
        let mut jobs = sample_jobs();
        jobs[0].state = "pid_dead".into();
        jobs[0].attachable = false;
        assert_eq!(
            command_for(
                &jobs,
                DaemonRowRef::Job { job_index: 0 },
                DaemonAction::Attach,
                None
            ),
            None
        );
        assert_eq!(
            command_for(
                &jobs,
                DaemonRowRef::Job { job_index: 0 },
                DaemonAction::Kill,
                None
            )
            .as_deref(),
            Some("kill 7")
        );
    }

    #[test]
    fn job_actions_cover_inspect_logs_wait_stop_gc() {
        let jobs = sample_jobs();
        let row = DaemonRowRef::Job { job_index: 0 };
        assert_eq!(
            command_for(&jobs, row, DaemonAction::Stop, None).as_deref(),
            Some("stop 7")
        );
        assert_eq!(
            command_for(&jobs, row, DaemonAction::Wait, None).as_deref(),
            Some("wait 7 --timeout 30")
        );
        assert_eq!(
            command_for(&jobs, row, DaemonAction::Inspect, None).as_deref(),
            Some("inspect 7")
        );
        assert_eq!(
            command_for(&jobs, row, DaemonAction::Logs, None).as_deref(),
            Some("logs 7 --last 100")
        );
        assert_eq!(
            command_for(&jobs, row, DaemonAction::Gc, None).as_deref(),
            Some("ps --gc --json")
        );
    }
}

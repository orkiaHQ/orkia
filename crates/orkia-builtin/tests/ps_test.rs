// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use orkia_builtin::ps::{self, PsAgent, PsModel, PsRow};
use orkia_shell_types::{
    AgentInfo, AgentStatus, BlockContent, DaemonJobView, DaemonStageView, JobId, JobInfo, JobKind,
    JobState, PsFlags,
};
use std::time::Duration;
use uuid::Uuid;

fn agent(name: &str) -> AgentInfo {
    AgentInfo {
        id: Uuid::nil(),
        name: name.into(),
        archetype: "engineer".into(),
        status: AgentStatus::Idle,
        model: "claude".into(),
        dir: std::path::PathBuf::new(),
        description: None,
        command: "claude".into(),
        args: Vec::new(),
        assigned_projects: Vec::new(),
        max_context_tokens: 4000,
    }
}

fn job(id: u32, name: &str) -> JobInfo {
    JobInfo {
        id: JobId(id),
        kind: JobKind::Agent {
            agent_id: Uuid::nil(),
            agent_name: name.into(),
        },
        state: JobState::Running,
        label: "claude".into(),
        pid: Some(8000),
        runtime: Duration::from_secs(125),
        sink: None,
    }
}

fn daemon_view(id: u32, state: &str, exit_code: Option<i32>) -> DaemonJobView {
    DaemonJobView {
        id,
        agent: "sage".into(),
        state: state.into(),
        pid: Some(4242),
        label: "claude".into(),
        runtime_secs: 90,
        exit_code,
        stages: Vec::new(),
    }
}

fn model(agents: Vec<PsAgent>, jobs: Vec<PsRow>) -> PsModel {
    PsModel {
        agents: Some(agents),
        jobs,
    }
}

#[test]
fn parses_flags() {
    let f = PsFlags::parse(&[]).unwrap();
    assert!(f.show_agents && f.show_system && !f.full && !f.json);

    let f = PsFlags::parse(&["--agents".into()]).unwrap();
    assert!(f.show_agents && !f.show_system);

    let f = PsFlags::parse(&["--system".into()]).unwrap();
    assert!(!f.show_agents && f.show_system);

    let f = PsFlags::parse(&["--full".into()]).unwrap();
    assert!(f.full);

    let f = PsFlags::parse(&["--json".into()]).unwrap();
    assert!(f.json);

    assert!(PsFlags::parse(&["--what".into()]).is_err());
    // shape gate routes them to brush, and the parser refuses them.
    assert!(PsFlags::parse(&["-a".into()]).is_err());
}

#[test]
fn agents_only_skips_system() {
    let flags = PsFlags {
        show_agents: true,
        show_system: false,
        full: false,
        json: false,
    };
    let m = model(
        vec![PsAgent::from_info(&agent("faye"))],
        vec![PsRow::from_job_info(&job(1, "faye"))],
    );
    let blocks = ps::render(&m, &flags);
    let has_processes_header = blocks
        .iter()
        .any(|b| matches!(b, BlockContent::SystemInfo(s) if s.contains("PROCESSES")));
    assert!(!has_processes_header);
    let has_agents_header = blocks
        .iter()
        .any(|b| matches!(b, BlockContent::SystemInfo(s) if s.contains("AGENTS")));
    assert!(has_agents_header);
    let has_resource_columns = blocks.iter().any(
        |b| matches!(b, BlockContent::SystemInfo(s) if s.contains("CPU") && s.contains("MEM")),
    );
    assert!(has_resource_columns);
}

#[test]
fn json_returns_single_block() {
    let flags = PsFlags {
        show_agents: true,
        show_system: false,
        full: false,
        json: true,
    };
    let m = model(
        vec![PsAgent::from_info(&agent("faye"))],
        vec![PsRow::from_job_info(&job(1, "faye"))],
    );
    let blocks = ps::render(&m, &flags);
    assert_eq!(blocks.len(), 1);
    let BlockContent::Text(payload) = &blocks[0] else {
        panic!("expected Text block");
    };
    assert!(payload.contains("\"faye\""));
    assert!(payload.contains("\"jobs\""));
    assert!(payload.contains("\"cpu_percent\""));
    assert!(payload.contains("\"mem_percent\""));
}

#[test]
fn json_omits_agents_key_when_frontend_has_no_roster() {
    // The CLI frontend passes `agents: None` — the key must be absent,
    let flags = PsFlags {
        show_agents: true,
        show_system: false,
        full: false,
        json: true,
    };
    let m = PsModel {
        agents: None,
        jobs: vec![PsRow::from_daemon_view(&daemon_view(7, "running", None))],
    };
    let blocks = ps::render(&m, &flags);
    let BlockContent::Text(payload) = &blocks[0] else {
        panic!("expected Text block");
    };
    assert!(!payload.contains("\"agents\""));
    assert!(payload.contains("\"jobs\""));
}

#[test]
fn no_jobs_shows_none_running() {
    let flags = PsFlags {
        show_agents: true,
        show_system: false,
        full: false,
        json: false,
    };
    let blocks = ps::render(&model(Vec::new(), Vec::new()), &flags);
    assert!(
        blocks
            .iter()
            .any(|b| matches!(b, BlockContent::SystemInfo(s) if s.contains("none running")))
    );
}

// These pins moved here from the REPL bridge tests when the duplicated
// `daemon_view_to_job_info` mapping collapsed into `ps::job_state`.

#[test]
fn running_and_detached_states_map_to_running() {
    for raw in ["running", "detached", "starting", "anything-else"] {
        assert_eq!(ps::job_state(raw, None), JobState::Running, "raw={raw}");
    }
}

#[test]
fn done_state_carries_recorded_exit_code() {
    assert_eq!(ps::job_state("done", None), JobState::Done { exit_code: 0 });
    assert_eq!(
        ps::job_state("done", Some(3)),
        JobState::Done { exit_code: 3 }
    );
}

#[test]
fn failed_state_preserves_reason() {
    assert_eq!(
        ps::job_state("failed: pty lost", None),
        JobState::Failed {
            reason: "failed: pty lost".into()
        }
    );
}

#[test]
fn daemon_view_scalar_fields_carry_through() {
    let row = PsRow::from_daemon_view(&daemon_view(42, "running", None));
    assert_eq!(row.id, 42);
    assert_eq!(row.agent, "sage");
    assert_eq!(row.state, "running");
    assert_eq!(row.pid, Some(4242));
    assert_eq!(row.label, "claude");
    assert_eq!(row.runtime_secs, 90);
    assert!(row.attachable);
    assert!(row.sink.is_none());
}

#[test]
fn done_daemon_view_is_not_attachable() {
    let row = PsRow::from_daemon_view(&daemon_view(1, "done", Some(0)));
    assert!(!row.attachable);
    assert_eq!(row.exit_code, Some(0));
}

#[test]
fn text_render_shows_raw_daemon_state_and_stage_lines() {
    // Text keeps the daemon's display vocabulary ("detached", not the
    // collapsed "running") and renders one indented line per stage with
    // the `job:stage` id form.
    let flags = PsFlags {
        show_agents: true,
        show_system: false,
        full: false,
        json: false,
    };
    let mut view = daemon_view(3, "detached", None);
    view.stages.push(DaemonStageView {
        id: 1,
        target: "@faye".into(),
        state: "running".into(),
        pid: Some(5151),
        runtime_secs: 12,
        exit_code: None,
        attachable: true,
    });
    let m = model(Vec::new(), vec![PsRow::from_daemon_view(&view)]);
    let blocks = ps::render(&m, &flags);
    let text: Vec<&str> = blocks
        .iter()
        .filter_map(|b| match b {
            BlockContent::Text(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert!(text.iter().any(|l| l.contains("detached")), "{text:?}");
    assert!(text.iter().any(|l| l.contains("3:1")), "{text:?}");
    assert!(text.iter().any(|l| l.contains("@faye")), "{text:?}");
}

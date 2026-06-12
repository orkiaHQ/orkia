// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Shared test scaffolding for PTY job tests.
//!
//! Spawns a fake agent through the production [`JobController::spawn`] path
//! (REF-006) — the same code production uses — instead of the old test-only
//! `JobController::spawn_agent` shim that duplicated the spawn logic. The
//! state-machine / injection-executor subsystems are stood up but left
//! unattached: these tests drive the detector channels themselves, matching
//! the lighter behaviour the removed shim provided.

#![allow(dead_code)] // each integration-test binary uses only a subset

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use orkia_shell::approval::ApprovalWatcher;
use orkia_shell::injection_executor::InjectionExecutor;
use orkia_shell::job::JobController;
use orkia_shell::job::config::{Attachment, JobConfig};
use orkia_shell::job::spawn::SpawnDeps;
use orkia_shell::protocol::EventRouter;
use orkia_shell::terminal_state::TerminalStateMachine;
use orkia_shell_types::{JobId, ProcessGroupMode, StdinSource};

/// Description of a fake agent to spawn. Only the fields the integration
/// tests actually vary; everything else uses the bare shell-job defaults.
pub struct FakeAgent<'a> {
    pub name: &'a str,
    pub cmd: &'a str,
    pub args: &'a [String],
    /// When set, an `Osc133Listener` is attached so OSC 133 + APC payloads
    /// route through this router. When `None`, no listener is wired.
    pub event_router: Option<&'a EventRouter>,
    /// When set, written to the PTY at spawn via `StdinSource::InitialBytes`
    /// (a trailing newline is appended if missing).
    pub initial_prompt: Option<&'a str>,
}

impl<'a> FakeAgent<'a> {
    /// A minimal fake agent: a command with args, no router, no prompt.
    pub fn cmd(name: &'a str, cmd: &'a str, args: &'a [String]) -> Self {
        FakeAgent {
            name,
            cmd,
            args,
            event_router: None,
            initial_prompt: None,
        }
    }
}

/// its job id.
pub fn spawn_fake_agent(jobs: &mut JobController, dir: &Path, spec: FakeAgent<'_>) -> JobId {
    let approvals = ApprovalWatcher::new(dir);
    let state_machine = TerminalStateMachine::new();
    let injection_executor = InjectionExecutor::spawn();
    let job_projects = Arc::new(parking_lot::RwLock::new(HashMap::new()));
    // `SpawnDeps` always needs a router reference; only an `Osc133Listener`
    // attachment actually consumes it, so a throwaway suffices when the test
    // does not supply one.
    let local_router = EventRouter::new();
    let router = spec.event_router.unwrap_or(&local_router);

    let mut attachments = Vec::new();
    if spec.event_router.is_some() {
        attachments.push(Attachment::Osc133Listener);
    }
    let stdin = match spec.initial_prompt {
        Some(p) => StdinSource::InitialBytes(p.as_bytes().to_vec()),
        None => StdinSource::Pty,
    };

    let config = JobConfig {
        command: spec.cmd,
        provider: orkia_shell_types::ProviderId::Generic,
        args: spec.args,
        label: format!("{} ({})", spec.name, spec.cmd),
        env: Vec::new(),
        working_dir: None,
        stdin,
        process_group: ProcessGroupMode::NewSession,
        attachments,
        cage_wrapper: None,
    };
    let deps = SpawnDeps {
        approvals: &approvals,
        event_router: router,
        state_machine: &state_machine,
        injection_executor: &injection_executor,
        job_projects: &job_projects,
        agent_name: spec.name,
    };
    jobs.spawn(config, deps).expect("spawn fake agent").job_id
}

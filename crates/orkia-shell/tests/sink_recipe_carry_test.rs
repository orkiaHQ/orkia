// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `@agent … | sink` construct must spawn DAEMON-owned, forwarding the
//! verbatim line so the detached runtime re-parses the same
//! `AgentToSink` shape and binds the sink recipe in-process — the whole
//! recipe (agent + per-turn sink) survives REPL exit.
//!
//! 1. With a `DetachedSpawner` installed (the main REPL), the sink line
//!    is forwarded verbatim — including `--once` — with the agent name.
//! 2. Without a spawner (a detached runtime — the recursion guard), the
//!    line stays in-process and never reaches the daemon.

use std::sync::{Arc, Mutex};

use orkia_shell::config::ShellConfig;
use orkia_shell::renderer::{PromptContext, RenderEvent, ShellRenderer};
use orkia_shell::{HeuristicClassifier, HeuristicRouter, Repl};
use orkia_shell_types::{DetachedSpawnRequest, DetachedSpawner};
use std::collections::HashMap;
use tempfile::TempDir;

#[derive(Default, Clone)]
struct TestRenderer;

impl ShellRenderer for TestRenderer {
    fn publish(&mut self, _event: RenderEvent) {}
    fn read_line(&mut self, _ctx: &PromptContext) -> Option<String> {
        None
    }
}

fn cfg(dir: &TempDir) -> ShellConfig {
    ShellConfig {
        data_dir: dir.path().to_path_buf(),
        agents: vec![],
        agent_commands: HashMap::new(),
        native_agents: Default::default(),
        default_shell: None,
        default_project: None,
        default_scope: None,
        default_mode: None,
        load_bashrc: None,
        load_profile: None,
        notification_verbosity: None,
        cage: Default::default(),
        daemon: Default::default(),
    }
}

/// Records every spawn request so the test can assert on the carried line.
#[derive(Default)]
struct RecordingSpawner {
    requests: Mutex<Vec<DetachedSpawnRequest>>,
}

impl DetachedSpawner for RecordingSpawner {
    fn spawn_detached(&self, req: DetachedSpawnRequest) -> Result<u32, String> {
        self.requests.lock().unwrap().push(req);
        Ok(7)
    }
}

#[tokio::test]
async fn sink_line_spawns_daemon_owned_with_verbatim_line() {
    let dir = TempDir::new().unwrap();
    let spawner = Arc::new(RecordingSpawner::default());
    let mut repl = Repl::new(
        TestRenderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg(&dir),
    )
    .with_detached_spawner(spawner.clone());

    repl.tick("@faye review the diff | tee out.txt".into())
        .await
        .unwrap();

    let reqs = spawner.requests.lock().unwrap();
    assert_eq!(reqs.len(), 1, "sink line must flip to the daemon");
    assert_eq!(reqs[0].command, "@faye review the diff | tee out.txt");
    assert_eq!(reqs[0].agent_name.as_deref(), Some("faye"));
}

#[tokio::test]
async fn once_sink_line_is_forwarded_verbatim() {
    let dir = TempDir::new().unwrap();
    let spawner = Arc::new(RecordingSpawner::default());
    let mut repl = Repl::new(
        TestRenderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg(&dir),
    )
    .with_detached_spawner(spawner.clone());

    repl.tick("@faye list TODOs --once | tee once.out".into())
        .await
        .unwrap();

    let reqs = spawner.requests.lock().unwrap();
    assert_eq!(reqs.len(), 1);
    // `--once` rides along verbatim: the runtime re-derives the one-shot
    // lifecycle from the same line.
    assert_eq!(reqs[0].command, "@faye list TODOs --once | tee once.out");
}

#[tokio::test]
async fn without_spawner_sink_line_stays_in_process() {
    let dir = TempDir::new().unwrap();
    // No spawner installed — the detached-runtime recursion guard. The
    // dispatch must fall through to the in-process sink path (which here
    // fails on the unconfigured agent rather than panicking or spawning).
    let mut repl = Repl::new(
        TestRenderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg(&dir),
    );
    repl.tick("@faye review | tee out.txt".into())
        .await
        .unwrap();
}

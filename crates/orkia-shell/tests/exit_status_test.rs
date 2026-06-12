// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Drives the real REPL (real brush) and asserts `echo $?` is truthful
//! across the builtin/brush boundary in both directions: a builtin's
//! usage error (2) / runtime error (1) / success (0) is visible to the
//! next shell line, and a brush command's own code overwrites it.

use std::collections::HashMap;

use orkia_shell::config::ShellConfig;
use orkia_shell::decision::BlockContent;
use orkia_shell::renderer::{PromptContext, RenderEvent, ShellRenderer};
use orkia_shell::{HeuristicClassifier, HeuristicRouter, Repl};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

#[derive(Default, Clone)]
struct TestRenderer {
    events: Arc<Mutex<Vec<RenderEvent>>>,
}

impl ShellRenderer for TestRenderer {
    fn publish(&mut self, event: RenderEvent) {
        self.events.lock().expect("lock").push(event);
    }
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

fn repl(dir: &TempDir) -> (Repl, Arc<Mutex<Vec<RenderEvent>>>) {
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(dir));
    (repl, events)
}

/// Run `echo $?` through brush and return true when its output carries a
/// line that is exactly `expected`. Events are cleared first so only this
/// tick's output is inspected.
async fn echo_status_is(
    repl: &mut Repl,
    events: &Arc<Mutex<Vec<RenderEvent>>>,
    expected: &str,
) -> bool {
    events.lock().expect("lock").clear();
    repl.tick("echo $?".into()).await.expect("tick ok");
    let collected = events.lock().expect("lock").clone();
    let mut text = String::new();
    for e in &collected {
        if let RenderEvent::Block(BlockContent::Text(t)) = e {
            text.push_str(t);
            text.push('\n');
        }
    }
    text.lines().any(|l| l.trim() == expected)
}

#[tokio::test]
async fn builtin_usage_error_then_echo_status_prints_2() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    // `orkia ps --bogus` claims the typed builtin and fails its flag
    repl.tick("orkia ps --bogus".into()).await.expect("tick ok");
    assert!(
        echo_status_is(&mut repl, &events, "2").await,
        "usage error must surface as $? == 2"
    );

    // A succeeding brush command overwrites `$?` naturally.
    repl.tick("true".into()).await.expect("tick ok");
    assert!(
        echo_status_is(&mut repl, &events, "0").await,
        "brush success must overwrite the stale 2"
    );
}

#[tokio::test]
async fn brush_failure_then_builtin_success_round_trips() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    // brush's own failure code is untouched by the bridge.
    repl.tick("false".into()).await.expect("tick ok");
    assert!(
        echo_status_is(&mut repl, &events, "1").await,
        "false must report $? == 1"
    );

    // A succeeding builtin (bare `ps`, the happy path) resets to 0 —
    // the seed crosses the builtin → brush boundary.
    repl.tick("orkia ps".into()).await.expect("tick ok");
    assert!(
        echo_status_is(&mut repl, &events, "0").await,
        "builtin success must surface as $? == 0"
    );
}

#[tokio::test]
async fn builtin_runtime_error_reports_1() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    // `stop` with an unmatched target is a runtime failure (the
    // invocation shape was fine) — code 1, not 2.
    repl.tick("orkia stop zz-no-such-job".into())
        .await
        .expect("tick ok");
    assert!(
        echo_status_is(&mut repl, &events, "1").await,
        "runtime builtin error must surface as $? == 1"
    );
}

#[tokio::test]
async fn bare_stop_usage_error_reports_2() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    // Missing required argument → usage-shaped → 2.
    repl.tick("orkia stop".into()).await.expect("tick ok");
    assert!(
        echo_status_is(&mut repl, &events, "2").await,
        "usage: stop … must surface as $? == 2"
    );
}

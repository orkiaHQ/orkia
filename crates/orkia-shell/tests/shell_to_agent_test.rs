// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Drives the REPL through the new `<shell> | @agent [body]` path:
//!
//! 1. Parser-level routing — `printf x | @a` becomes
//!    `Decision::ShellToAgent` and exits the dispatcher cleanly.
//! 2. Real PTY round-trip — when the spawn succeeds with `cat -u` as
//!    the fake agent, the composed body is delivered through the
//!    detector-gated injection path and lands on the agent's PTY
//!    (proving the wiring is correct).
//! 3. Negative paths — multi-agent, agent-on-left, empty agent name,
//!    non-zero shell exit, all return clear errors and do *not* spawn
//!    a job.
//! 4. Coexistence — `cat a | grep b` (no `@`) still routes to brush
//!    as a plain POSIX pipeline.

use std::collections::HashMap;
use std::time::Duration;

use orkia_shell::config::{AgentCommandConfig, ShellConfig};
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

/// Build a config with a single agent named `cat-pipe` whose runtime
/// is `cat -u`. `cat -u` echoes every byte it receives on stdin to
/// stdout unbuffered — the ideal stand-in for "did the agent get the
/// bytes we sent it?" without bringing in claude/codex/gemini.
fn cfg_with_cat_agent(dir: &TempDir) -> ShellConfig {
    let mut agent_commands = HashMap::new();
    agent_commands.insert(
        "cat-pipe".to_string(),
        AgentCommandConfig {
            command: "cat".to_string(),
            args: vec!["-u".to_string()],
        },
    );
    ShellConfig {
        data_dir: dir.path().to_path_buf(),
        agents: vec![],
        agent_commands,
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

fn cfg_no_agent(dir: &TempDir) -> ShellConfig {
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

fn collect_text(events: &[RenderEvent]) -> String {
    let mut s = String::new();
    for e in events {
        if let RenderEvent::Block(
            BlockContent::Text(t) | BlockContent::SystemInfo(t) | BlockContent::Error(t),
        ) = e
        {
            s.push_str(t);
            s.push('\n');
        }
    }
    s
}

// ── Parser routing ────────────────────────────────────────────────

#[tokio::test]
async fn multi_agent_pipeline_rejected_with_team_message() {
    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(
        renderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg_no_agent(&dir),
    );

    repl.tick("foo | @a | @b".into()).await.expect("tick ok");
    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        text.contains("requires Orkia Team"),
        "expected Team-required message, got: {text}"
    );
}

#[tokio::test]
async fn agent_to_agent_pipeline_is_team_only() {
    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(
        renderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg_no_agent(&dir),
    );

    repl.tick("@a | @b".into()).await.expect("tick ok");
    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        text.contains("Orkia Team"),
        "expected Team-required message for agent-to-agent pipeline, got: {text}"
    );
}

#[tokio::test]
async fn agent_into_shell_in_mixed_pipeline_rejected() {
    // `foo | @a | grep bar` — the agent stage `@a` ends up piping
    // into a downstream command. An interactive agent emits a
    // ByteStream (rendered TUI), not the structured input a command
    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(
        renderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg_no_agent(&dir),
    );

    repl.tick("printf x | @a | grep bar".into())
        .await
        .expect("tick ok");
    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        text.contains("type mismatch") && text.contains("@a"),
        "expected agent-on-left TypeMismatch rejection, got: {text}"
    );
}

#[tokio::test]
async fn missing_agent_name_rejected() {
    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(
        renderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg_no_agent(&dir),
    );

    repl.tick("printf foo | @".into()).await.expect("tick ok");
    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        text.contains("missing agent name"),
        "expected missing-agent-name rejection, got: {text}"
    );
}

#[tokio::test]
async fn plain_posix_pipe_still_routes_to_shell() {
    // `printf hi | tr a-z A-Z` has no `@` on the right — the shell
    // engine handles it as a regular POSIX pipeline. The output
    // should be uppercase HI somewhere in the captured text.
    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(
        renderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg_no_agent(&dir),
    );

    repl.tick("printf hi | tr a-z A-Z".into())
        .await
        .expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    // brush's PTY output gets surfaced as a Text block — `HI` lands
    // somewhere in there. We don't assert exact byte equality
    // because PTY echoes the command line back too.
    assert!(
        text.contains("HI"),
        "expected uppercase output, got: {text}"
    );
}

// ── Negative shell exit ───────────────────────────────────────────

#[tokio::test]
async fn nonzero_shell_exit_short_circuits_agent_spawn() {
    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(
        renderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg_with_cat_agent(&dir),
    );

    // `false` exits 1, produces no stdout. Agent should not spawn.
    repl.tick("false | @cat-pipe".into())
        .await
        .expect("tick ok");
    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        text.contains("shell prefix failed (exit 1)"),
        "expected shell-failed error, got: {text}"
    );
    // "spawned as background" / "[N] spawned" never appears when the
    // shell stage short-circuits. The literal "spawned" appears only
    // inside the error string ("agent not spawned"), which we filter
    // out before asserting absence.
    let positive_spawn_signal = text
        .lines()
        .any(|l| l.contains("spawned") && !l.contains("not spawned"));
    assert!(
        !positive_spawn_signal,
        "agent must not have spawned, got: {text}"
    );
}

#[tokio::test]
async fn unknown_agent_returns_clear_error() {
    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(
        renderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg_no_agent(&dir),
    );

    repl.tick("printf foo | @nonexistent".into())
        .await
        .expect("tick ok");
    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        text.contains("no command configured"),
        "expected unknown-agent error, got: {text}"
    );
}

// ── Real PTY round-trip ───────────────────────────────────────────

#[tokio::test]
async fn successful_pipe_spawns_job() {
    // The full happy path: `printf foo | @cat-pipe`.
    // - Shell stage exits 0
    // - Agent has a configured runtime (`cat -u`)
    // - Dispatch must reach the JobController spawn and produce a
    //   JobSpawned system-info block (not an error).
    // PTY-byte round-trip itself is exercised by byte_inject_test.rs;
    // here we only confirm composition + the detector-gated injection
    // wiring did not error out before reaching spawn.
    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(
        renderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg_with_cat_agent(&dir),
    );

    repl.tick("printf foo | @cat-pipe review".into())
        .await
        .expect("tick ok");

    // Let the spawn lifecycle settle.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        text.contains("spawned"),
        "expected job-spawned info block, got: {text}"
    );
    assert!(
        !text.contains("failed to spawn"),
        "spawn must not have errored, got: {text}"
    );
}

#[tokio::test]
async fn history_records_shell_to_agent_type() {
    use orkia_shell_types::HistoryType;

    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let mut repl = Repl::new(
        renderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg_with_cat_agent(&dir),
    );

    repl.tick("printf 'x' | @cat-pipe".into())
        .await
        .expect("tick ok");

    let entries = repl.history_snapshot();
    let last = entries.last().expect("at least one history entry");
    assert_eq!(last.entry_type, HistoryType::ShellToAgent);
    assert_eq!(last.agent.as_deref(), Some("cat-pipe"));
}

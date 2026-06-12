// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Drives the real REPL through `tick` and asserts where each
//! operator-bearing line lands: brush (collidable head), loud refusal
//! (native head), typed external_suffix (untouched), bang passthrough,
//! and the in-process `orkia <unknown>` error. The scanner and the
//! Brush/Refuse verdicts themselves are unit-tested in
//! `src/operator_routing.rs`; this file proves the REPL wiring.

use std::collections::HashMap;

use orkia_shell::config::ShellConfig;
use orkia_shell::decision::BlockContent;
use orkia_shell::renderer::{PromptContext, RenderEvent, ShellRenderer};
use orkia_shell::{HeuristicClassifier, HeuristicRouter, Repl};
use orkia_shell_types::HistoryType;
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

const REFUSAL: &str = "does not support shell operators";

// ── 1. Collidable head + pipe → brush, builtin never runs ──────────

#[tokio::test]
async fn collidable_ps_pipe_routes_whole_line_to_brush() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    // System `ps aux` headers carry USER/%CPU — the orkia builtin's
    // table never does. `head -1` keeps the output to the header line.
    repl.tick("ps aux | head -1".into()).await.expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        text.contains("USER") || text.contains("PID"),
        "expected system ps header via brush, got: {text}"
    );
    assert!(!text.contains(REFUSAL), "must not refuse: {text}");
    let entries = repl.history_snapshot();
    let last = entries.last().expect("history");
    assert_eq!(
        last.entry_type,
        HistoryType::Shell,
        "decision must be Shell (brush), not Builtin"
    );
}

// ── 2. The silent-drop regression: login + pipe never invokes auth ──

#[tokio::test]
async fn collidable_login_pipe_never_invokes_orkia_auth() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    // An invalid flag makes /usr/bin/login exit immediately with a
    // usage error instead of prompting — the routing is what's under
    // test, not login itself. stderr is dropped to keep output stable.
    repl.tick("login -zz-bogus 2>/dev/null | grep x".into())
        .await
        .expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    // The orkia auth builtin renders login/session blocks; none may
    // appear, and the line must not be refused — it went to brush.
    assert!(!text.contains(REFUSAL), "must not refuse: {text}");
    // The auth arm in this harness (no provider wired) would print
    // "login: no auth provider configured" — its absence proves the
    // builtin never dispatched.
    assert!(
        !text.contains("no auth provider"),
        "orkia auth must not have been invoked, got: {text}"
    );
    let entries = repl.history_snapshot();
    let last = entries.last().expect("history");
    assert_eq!(
        last.entry_type,
        HistoryType::Shell,
        "login|pipe must route to brush, not the auth builtin"
    );
}

// ── 3. Native head + pipe → loud refusal, no side effect ───────────

#[tokio::test]
async fn native_tell_pipe_is_refused_without_side_effect() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    repl.tick("tell @a hi | grep x".into())
        .await
        .expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        text.contains(REFUSAL) && text.contains("tell"),
        "expected operator refusal naming tell, got: {text}"
    );
    // The tell handler never ran: its own errors ("tell: missing…",
    // "no agent/job…") must be absent — refusal happens before dispatch.
    assert!(
        !text.contains("tell: missing") && !text.contains("no agent"),
        "tell handler must not have run, got: {text}"
    );
}

// ── 4. Native head + && → refused, nothing executes ────────────────

#[tokio::test]
async fn native_setup_and_chain_is_refused_entirely() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    repl.tick("setup && echo done".into())
        .await
        .expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        text.contains(REFUSAL) && text.contains("setup"),
        "expected operator refusal naming setup, got: {text}"
    );
    assert!(
        !text.contains("done"),
        "echo tail must not run either — the whole line is refused: {text}"
    );
}

// ── 5. Typed path untouched: orkia jobs | grep x ────────────────────

#[tokio::test]
async fn typed_external_suffix_path_is_untouched() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    repl.tick("orkia jobs | grep zz-no-such-job".into())
        .await
        .expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        !text.contains(REFUSAL),
        "typed path must not refuse: {text}"
    );
    assert!(
        !text.contains("unknown builtin"),
        "typed path must parse, got: {text}"
    );
    let entries = repl.history_snapshot();
    let last = entries.last().expect("history");
    assert_eq!(
        last.entry_type,
        HistoryType::Builtin,
        "orkia jobs | grep must stay on the typed exec path"
    );
}

// ── 6. `!` wins: bang line is a brush byte pipeline, no agent ───────

#[tokio::test]
async fn bang_line_with_agent_pipe_runs_in_brush() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    repl.tick("!echo hi | @faye".into()).await.expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    // brush execs `@faye` as a command → not found; no agent spawn, no
    // Team-required message (which the pipeline path would emit).
    assert!(
        !text.contains("spawned") && !text.contains("Team"),
        "no agent path may trigger on a bang line, got: {text}"
    );
    let entries = repl.history_snapshot();
    let last = entries.last().expect("history");
    assert_eq!(
        last.entry_type,
        HistoryType::Shell,
        "bang line must record as shell passthrough"
    );
}

// ── 7. orkia <unknown> errors in-process ────────────────────────────

#[tokio::test]
async fn orkia_unknown_errors_in_process_with_candidates() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    repl.tick("orkia nosuchcmd".into()).await.expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        text.contains("unknown builtin: nosuchcmd"),
        "expected in-process unknown-builtin error, got: {text}"
    );
    // The brush/child path would print a 127 "command not found" —
    // its absence (plus the error block above) proves no child spawn.
    assert!(
        !text.contains("command not found"),
        "no child orkia may be spawned via brush, got: {text}"
    );
    let entries = repl.history_snapshot();
    let last = entries.last().expect("history");
    assert_eq!(
        last.entry_type,
        HistoryType::Builtin,
        "orkia <unknown> must resolve as a builtin decision"
    );
}

// ── 8. Quoting: operators inside quotes are arguments ───────────────

#[tokio::test]
async fn quoted_pipe_in_tell_body_dispatches_normally() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    repl.tick(r#"tell @a "x | y""#.into())
        .await
        .expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        !text.contains(REFUSAL),
        "quoted pipe is not an operator, got: {text}"
    );
    // tell ran (and failed on the unknown agent) — its own error shape.
    assert!(
        text.contains("tell"),
        "tell handler must have dispatched, got: {text}"
    );
}

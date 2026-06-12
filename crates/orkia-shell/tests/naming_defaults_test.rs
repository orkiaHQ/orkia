// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Drives the real REPL through `tick` and asserts the renamed command
//! family and the corrected bare defaults: `attach` is target-gated
//! (explicit `@name`/`%n`/`N:@name` only, usage error otherwise, zero
//! side effects), bare `route`/`login`/`log` reach brush while their
//! `orkia `-namespaced forms reach the Orkia handlers, and
//! `connect`/`disconnect` dispatch to the backend arms. Bare-name
//! classification itself is pinned in `classifier_test.rs`.

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

const ATTACH_USAGE: &str = "usage: attach @name | %n | N:@name";

// ── 1. attach target gating: usage error, zero side effects ────────

#[tokio::test]
async fn bare_attach_is_a_usage_error_with_no_side_effect() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    repl.tick("attach".into()).await.expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        text.contains(ATTACH_USAGE),
        "expected the gating usage error, got: {text}"
    );
    // No resolution ran: the "no job matching" path is the side of the
    // gate that touches job state — it must be absent.
    assert!(
        !text.contains("no job matching"),
        "bare attach must not resolve anything, got: {text}"
    );
    let entries = repl.history_snapshot();
    let last = entries.last().expect("history");
    assert_eq!(
        last.entry_type,
        HistoryType::Builtin,
        "attach is a builtin decision — never brush, never contextual"
    );
}

#[tokio::test]
async fn sigil_less_and_numeric_attach_targets_are_rejected() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    for line in ["attach faye", "attach 3", "attach 7:2"] {
        repl.tick(line.into()).await.expect("tick ok");
    }

    let text = collect_text(&events.lock().expect("lock"));
    assert_eq!(
        text.matches(ATTACH_USAGE).count(),
        3,
        "every non-explicit target form is gated, got: {text}"
    );
    assert!(
        !text.contains("no job matching"),
        "gated targets must never reach resolution, got: {text}"
    );
}

#[tokio::test]
async fn explicit_attach_targets_pass_the_gate() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    // `@name`, `%n`, and the stage form `N:@name` all pass the shape
    // gate and reach resolution (which misses — nothing is running).
    for line in ["attach @ghost", "attach %999", "attach 1:@sage"] {
        repl.tick(line.into()).await.expect("tick ok");
    }

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        !text.contains(ATTACH_USAGE),
        "explicit forms must not be gated, got: {text}"
    );
    assert_eq!(
        text.matches("no job matching").count(),
        3,
        "all three explicit forms reach resolution, got: {text}"
    );
}

// ── 2. bare route → brush; orkia route → typed routing table ───────

#[tokio::test]
async fn bare_route_goes_to_brush_not_the_routing_table() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    repl.tick("route".into()).await.expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        !text.contains("ROUTING TABLE"),
        "bare route belongs to the system binary, got: {text}"
    );
    let entries = repl.history_snapshot();
    let last = entries.last().expect("history");
    assert_eq!(
        last.entry_type,
        HistoryType::Shell,
        "bare route must record as a shell decision"
    );
}

#[tokio::test]
async fn namespaced_route_reaches_the_typed_routing_table() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    repl.tick("orkia route".into()).await.expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        text.contains("ROUTING TABLE"),
        "orkia route must reach the typed builtin, got: {text}"
    );
    let entries = repl.history_snapshot();
    let last = entries.last().expect("history");
    assert_eq!(last.entry_type, HistoryType::Builtin);
}

// ── 3. bare login → brush; orkia login → auth handler ──────────────

#[tokio::test]
async fn bare_login_goes_to_brush_not_orkia_auth() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    // An invalid flag makes /usr/bin/login exit immediately with a usage
    // error instead of prompting — the routing is what's under test.
    repl.tick("login -zz-bogus 2>/dev/null".into())
        .await
        .expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    // The no-provider harness would print "login: no auth provider
    // configured" — its absence proves the auth arm never dispatched.
    assert!(
        !text.contains("no auth provider"),
        "bare login must never invoke orkia auth, got: {text}"
    );
    let entries = repl.history_snapshot();
    let last = entries.last().expect("history");
    assert_eq!(last.entry_type, HistoryType::Shell);
}

#[tokio::test]
async fn namespaced_login_reaches_the_auth_handler() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    repl.tick("orkia login".into()).await.expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        text.contains("no auth provider"),
        "orkia login must dispatch the auth arm (no provider in this \
         harness), got: {text}"
    );
    let entries = repl.history_snapshot();
    let last = entries.last().expect("history");
    assert_eq!(last.entry_type, HistoryType::Builtin);
}

// ── 4. bare log → brush; orkia log → typed agent-log table ─────────

#[tokio::test]
async fn bare_log_goes_to_brush() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    // macOS: /usr/bin/log prints its own usage. Linux without a `log`
    // binary: an honest brush not-found. Either way the decision is
    // Shell and the Orkia job-log handler never runs.
    repl.tick("log".into()).await.expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        !text.contains("no output log for job"),
        "bare log must not reach the orkia job-log handler, got: {text}"
    );
    let entries = repl.history_snapshot();
    let last = entries.last().expect("history");
    assert_eq!(last.entry_type, HistoryType::Shell);
}

#[tokio::test]
async fn namespaced_log_reaches_the_typed_handler() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    repl.tick("orkia log 99".into()).await.expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        text.contains("no output log for job 99"),
        "orkia log must reach the typed handler, got: {text}"
    );
    let entries = repl.history_snapshot();
    let last = entries.last().expect("history");
    assert_eq!(last.entry_type, HistoryType::Builtin);
}

// ── 5. connect / disconnect dispatch to the backend arms ───────────

#[tokio::test]
async fn connect_and_disconnect_dispatch_to_backend_arms() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    repl.tick("connect".into()).await.expect("tick ok");
    repl.tick("disconnect".into()).await.expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        text.contains("connect: not connected"),
        "connect must dispatch its status arm, got: {text}"
    );
    assert!(
        text.contains("disconnect: nothing to do"),
        "disconnect must dispatch its arm, got: {text}"
    );
}

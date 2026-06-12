// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! matrix. Drives the real REPL through `tick` and asserts, per row,
//! which side of the parse-or-fallback gate the line lands on: builtin
//! grammar → Orkia handler (`HistoryType::Builtin`), POSIX shape →
//! brush verbatim (`HistoryType::Shell`, builtin never runs). The
//! grammar verdicts themselves are unit-pinned in `builtin_table.rs`;
//! this file pins the two routing seams (typed head + tick arm).

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

fn last_history(repl: &Repl) -> HistoryType {
    repl.history_snapshot()
        .last()
        .expect("history entry")
        .entry_type
        .clone()
}

// ── ps: bare + long flags → builtin; POSIX shapes → brush ──────────

#[tokio::test]
async fn bare_ps_runs_the_builtin() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    repl.tick("ps".into()).await.expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        !text.contains("unknown flag"),
        "bare ps is the builtin's happy path, got: {text}"
    );
    assert_eq!(last_history(&repl), HistoryType::Builtin);
}

#[tokio::test]
async fn ps_long_flags_run_the_builtin() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    repl.tick("ps --json".into()).await.expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        !text.contains("unknown flag"),
        "--json is declared grammar, got: {text}"
    );
    assert_eq!(last_history(&repl), HistoryType::Builtin);
}

#[tokio::test]
async fn posix_shaped_ps_falls_to_brush() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    for line in ["ps aux", "ps -a", "ps -p 1"] {
        repl.tick(line.into()).await.expect("tick ok");
        assert_eq!(
            last_history(&repl),
            HistoryType::Shell,
            "'{line}' must yield to the system ps"
        );
    }

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        !text.contains("ps: unknown flag"),
        "the builtin flag parser must never see a POSIX shape, got: {text}"
    );
}

// ── whoami: bare → builtin incl. system username; args → brush ─────

#[tokio::test]
async fn bare_whoami_prints_the_system_username() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    repl.tick("whoami".into()).await.expect("tick ok");

    let expected = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".into());
    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        text.contains(&expected),
        "builtin whoami must include the system answer '{expected}', got: {text}"
    );
    assert_eq!(last_history(&repl), HistoryType::Builtin);
}

#[tokio::test]
async fn whoami_with_args_falls_to_brush() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, _events) = repl(&dir);

    // `/usr/bin/whoami` owns every argued form (`whoami -u` errors
    // there with its own usage — the routing is what's under test).
    repl.tick("whoami -u 2>/dev/null".into())
        .await
        .expect("tick ok");

    assert_eq!(last_history(&repl), HistoryType::Shell);
}

// ── audit: bare + filter grammar → builtin; alien shapes → brush ───

#[tokio::test]
async fn bare_audit_runs_the_builtin() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, _events) = repl(&dir);

    repl.tick("audit".into()).await.expect("tick ok");

    assert_eq!(last_history(&repl), HistoryType::Builtin);
}

#[tokio::test]
async fn posix_shaped_audit_falls_to_brush() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, _events) = repl(&dir);

    // `-e` is the BSM audit tool's territory (macOS /usr/sbin/audit);
    // short flags are outside the builtin grammar everywhere.
    repl.tick("audit -e 2>/dev/null".into())
        .await
        .expect("tick ok");

    assert_eq!(last_history(&repl), HistoryType::Shell);
}

// ── log: %n/@agent → typed builtin; words → brush ──────────────────

#[tokio::test]
async fn log_with_job_sigil_reaches_the_typed_handler() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    repl.tick("log %1".into()).await.expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        text.contains("no job matching"),
        "log %1 must reach the orkia job-log handler, got: {text}"
    );
    assert_eq!(last_history(&repl), HistoryType::Builtin);
}

#[tokio::test]
async fn log_with_bare_word_falls_to_brush() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    // A bare word (`log show`, `log stream`, …) is the system log's
    // grammar. A bogus word keeps /usr/bin/log fast on macOS and is an
    // honest not-found elsewhere — either way the decision is Shell.
    repl.tick("log zz-bogus-verb 2>/dev/null".into())
        .await
        .expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        !text.contains("no output log") && !text.contains("invalid target"),
        "word-shaped log must never reach the orkia handler, got: {text}"
    );
    assert_eq!(last_history(&repl), HistoryType::Shell);
}

// ── route: show → typed builtin; system shapes → brush ─────────────

#[tokio::test]
async fn route_show_reaches_the_typed_routing_table() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    repl.tick("route show".into()).await.expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        text.contains("ROUTING TABLE"),
        "route show is declared grammar, got: {text}"
    );
    assert_eq!(last_history(&repl), HistoryType::Builtin);
}

#[tokio::test]
async fn posix_shaped_route_falls_to_brush() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    repl.tick("route -zz-bogus 2>/dev/null".into())
        .await
        .expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        !text.contains("ROUTING TABLE"),
        "flag-shaped route belongs to the system binary, got: {text}"
    );
    assert_eq!(last_history(&repl), HistoryType::Shell);
}

// ── kill: internal fallback unchanged (Inv 4) ──────────────────────

#[tokio::test]
async fn kill_with_unknown_pid_takes_the_system_path() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    // No grammar gating on kill: the builtin dispatches and its own
    // resolver falls through to `kill -TERM <pid>` for unknown targets.
    repl.tick("kill 999999".into()).await.expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        text.contains("kill -TERM"),
        "unknown pid must fall through to the system kill, got: {text}"
    );
    assert_eq!(last_history(&repl), HistoryType::Builtin);
}

// ── escape hatches stay intact (Inv 7) ─────────────────────────────

#[tokio::test]
async fn bang_forces_the_system_ps() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    repl.tick("!ps aux".into()).await.expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        !text.contains("unknown flag"),
        "! must bypass the builtin entirely, got: {text}"
    );
    assert_eq!(last_history(&repl), HistoryType::Shell);
}

#[tokio::test]
async fn run_forces_the_system_ps() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    repl.tick("run ps aux".into()).await.expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        !text.contains("unknown flag"),
        "run must bypass the builtin entirely, got: {text}"
    );
}

// ── namespace claim beats shape (doctrine) ─────────────────────────

#[tokio::test]
async fn namespaced_head_never_yields_to_brush() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = repl(&dir);

    // `orkia ps aux` claims the typed builtin: the shape gate must not
    // reroute it (the typed engine tolerates the excess positional).
    repl.tick("orkia ps aux".into()).await.expect("tick ok");
    assert_eq!(
        last_history(&repl),
        HistoryType::Builtin,
        "namespaced lines bypass shape routing"
    );

    // A flag-shaped alien token is the loud-failure side of the claim:
    // the builtin's own parser refuses it — never a silent brush reroute.
    repl.tick("orkia ps --bogus".into()).await.expect("tick ok");

    let text = collect_text(&events.lock().expect("lock"));
    assert!(
        text.contains("unknown flag"),
        "namespaced alien flags error in the builtin, got: {text}"
    );
}

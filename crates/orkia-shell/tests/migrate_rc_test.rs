// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Integration tests for `orkia migrate-rc` driven through the REPL.
//!
//! These exercise the wiring: builtin dispatch → handler → file write.
//! Parser correctness is covered in `orkia-builtin`'s unit tests; here
//! we care that `tick("migrate-rc ...")` actually writes a `.orkiarc`,
//! that `--dry-run` doesn't, and that `--append` extends rather than
//! truncates.
//!
//! Tests in this file mutate `$HOME`; they serialise on a local mutex
//! and allow `await_holding_lock` for the same reason the
//! `rc_loading_test.rs` file does.
#![allow(clippy::await_holding_lock)]

use std::fs;
use std::sync::Mutex;

use orkia_shell::config::ShellConfig;
use orkia_shell::renderer::{PromptContext, RenderEvent, ShellRenderer};
use orkia_shell::{HeuristicClassifier, HeuristicRouter, Repl};
use tempfile::TempDir;

static HOME_LOCK: Mutex<()> = Mutex::new(());
fn lock_home() -> std::sync::MutexGuard<'static, ()> {
    match HOME_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

#[derive(Default, Clone)]
struct CapturingRenderer {
    events: std::sync::Arc<std::sync::Mutex<Vec<RenderEvent>>>,
}

impl ShellRenderer for CapturingRenderer {
    fn publish(&mut self, e: RenderEvent) {
        self.events.lock().expect("lock").push(e);
    }
    fn read_line(&mut self, _ctx: &PromptContext) -> Option<String> {
        None
    }
}

fn cfg(dir: &TempDir) -> ShellConfig {
    ShellConfig {
        data_dir: dir.path().to_path_buf(),
        agents: vec![],
        agent_commands: std::collections::HashMap::new(),
        native_agents: Default::default(),
        default_shell: None,
        default_project: None,
        default_scope: None,
        default_mode: None,
        load_bashrc: Some(false),
        load_profile: Some(false),
        notification_verbosity: None,
        cage: Default::default(),
        daemon: Default::default(),
    }
}

#[tokio::test]
async fn migrate_rc_writes_orkiarc_from_explicit_zshrc() {
    let _g = lock_home();
    let home = TempDir::new().expect("tmp home");
    // SAFETY: process-wide env mutation is serialized on `lock_home()`
    // (held by `_g` above) so no other test reads/writes HOME
    // concurrently for the lifetime of this test.
    unsafe {
        std::env::set_var("HOME", home.path());
    }
    fs::write(
        home.path().join(".zshrc"),
        "export EDITOR=helix\nalias gs='git status'\nsetopt autocd\n",
    )
    .expect("write zshrc");

    let data_dir = TempDir::new().expect("tmp data");
    let mut repl = Repl::new(
        CapturingRenderer::default(),
        HeuristicClassifier,
        HeuristicRouter,
        cfg(&data_dir),
    );
    let from = home.path().join(".zshrc");
    repl.tick(format!("migrate-rc --from {}", from.display()))
        .await
        .expect("tick");

    let body = fs::read_to_string(home.path().join(".orkiarc"))
        .expect("orkiarc should exist after migrate-rc");
    assert!(body.contains("export EDITOR=helix"), "got:\n{body}");
    assert!(body.contains("alias gs='git status'"), "got:\n{body}");
    // setopt is zsh-only — must not appear as a live command.
    assert!(
        !body.contains("\nsetopt autocd\n"),
        "setopt should be skipped, got:\n{body}"
    );
    // ... but it should appear in the SKIP trailer.
    assert!(
        body.contains("# SKIP (ZshSetopt): setopt autocd"),
        "expected SKIP trailer for setopt, got:\n{body}"
    );
}

#[tokio::test]
async fn migrate_rc_dry_run_does_not_write() {
    let _g = lock_home();
    let home = TempDir::new().expect("tmp home");
    // SAFETY: process-wide env mutation is serialized on `lock_home()`
    // (held by `_g` above) so no other test reads/writes HOME
    // concurrently for the lifetime of this test.
    unsafe {
        std::env::set_var("HOME", home.path());
    }
    fs::write(home.path().join(".bashrc"), "export FOO=bar\n").expect("write bashrc");

    let data_dir = TempDir::new().expect("tmp data");
    let mut repl = Repl::new(
        CapturingRenderer::default(),
        HeuristicClassifier,
        HeuristicRouter,
        cfg(&data_dir),
    );
    let from = home.path().join(".bashrc");
    repl.tick(format!("migrate-rc --from {} --dry-run", from.display()))
        .await
        .expect("tick");

    assert!(
        !home.path().join(".orkiarc").exists(),
        "--dry-run must not write .orkiarc"
    );
}

#[tokio::test]
async fn migrate_rc_append_extends_existing() {
    let _g = lock_home();
    let home = TempDir::new().expect("tmp home");
    // SAFETY: process-wide env mutation is serialized on `lock_home()`
    // (held by `_g` above) so no other test reads/writes HOME
    // concurrently for the lifetime of this test.
    unsafe {
        std::env::set_var("HOME", home.path());
    }
    fs::write(
        home.path().join(".orkiarc"),
        "# pre-existing\nexport ALREADY_THERE=1\n",
    )
    .expect("write orkiarc");
    fs::write(home.path().join(".bashrc"), "export FROM_BASHRC=2\n").expect("write bashrc");

    let data_dir = TempDir::new().expect("tmp data");
    let mut repl = Repl::new(
        CapturingRenderer::default(),
        HeuristicClassifier,
        HeuristicRouter,
        cfg(&data_dir),
    );
    let from = home.path().join(".bashrc");
    repl.tick(format!("migrate-rc --from {} --append", from.display()))
        .await
        .expect("tick");

    let body = fs::read_to_string(home.path().join(".orkiarc")).expect("read");
    assert!(
        body.contains("ALREADY_THERE=1"),
        "pre-existing content must survive append, got:\n{body}"
    );
    assert!(
        body.contains("FROM_BASHRC=2"),
        "appended content must be present, got:\n{body}"
    );
}

#[tokio::test]
async fn migrate_rc_does_not_touch_source_file() {
    let _g = lock_home();
    let home = TempDir::new().expect("tmp home");
    // SAFETY: process-wide env mutation is serialized on `lock_home()`
    // (held by `_g` above) so no other test reads/writes HOME
    // concurrently for the lifetime of this test.
    unsafe {
        std::env::set_var("HOME", home.path());
    }
    let zshrc_path = home.path().join(".zshrc");
    let original = "export EDITOR=helix\nsetopt autocd\n";
    fs::write(&zshrc_path, original).expect("write zshrc");

    let data_dir = TempDir::new().expect("tmp data");
    let mut repl = Repl::new(
        CapturingRenderer::default(),
        HeuristicClassifier,
        HeuristicRouter,
        cfg(&data_dir),
    );
    repl.tick(format!("migrate-rc --from {}", zshrc_path.display()))
        .await
        .expect("tick");

    let after = fs::read_to_string(&zshrc_path).expect("read zshrc");
    assert_eq!(
        after, original,
        "source file must never be modified by migrate-rc"
    );
}

#[tokio::test]
async fn migrate_rc_unknown_flag_emits_error_block() {
    let _g = lock_home();
    let home = TempDir::new().expect("tmp home");
    // SAFETY: process-wide env mutation is serialized on `lock_home()`
    // (held by `_g` above) so no other test reads/writes HOME
    // concurrently for the lifetime of this test.
    unsafe {
        std::env::set_var("HOME", home.path());
    }
    let data_dir = TempDir::new().expect("tmp data");
    let renderer = CapturingRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(
        renderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg(&data_dir),
    );
    repl.tick("migrate-rc --bogus-flag".into())
        .await
        .expect("tick");

    let events = events.lock().expect("lock");
    let saw_error = events.iter().any(|e| {
        matches!(
            e,
            RenderEvent::Block(orkia_shell::decision::BlockContent::Error(m))
                if m.contains("migrate-rc") && m.contains("bogus")
        )
    });
    assert!(saw_error, "expected an error block, got {events:?}");
}

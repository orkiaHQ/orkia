// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Finding #2 / T2.54a — `rfc show <slug>` resolves project-less.
//!
//! A slug is an identity: `rfc show auth-refresh` names the RFC. When no
//! project context resolves (flag / default / cwd / `rfc cd`), `rfc show`
//! falls back to a workspace-wide slug lookup, mirroring `rfc list`:
//!
//! - unique slug across the workspace → renders project-less
//! - collision (same slug in N projects) → refuses, lists candidates,
//!   never guesses (fail-closed)
//! - `--project` always disambiguates
//! - unknown slug → "not found in any project"
//!
//! Drives the real REPL via `tick` against a tempdir — no network, no agent.
//! `EDITOR=true` makes `rfc create`'s editor spawn a clean no-op.

use std::sync::{Arc, Mutex};

use orkia_shell::config::ShellConfig;
use orkia_shell::decision::BlockContent;
use orkia_shell::renderer::{PromptContext, RenderEvent, ShellRenderer};
use orkia_shell::seal::{JobProjects, SealManager, spawn_consumer};
use orkia_shell::{HeuristicClassifier, HeuristicRouter, Repl};
use parking_lot::RwLock;
use tempfile::TempDir;

#[derive(Default, Clone)]
struct CapturingRenderer {
    events: Arc<Mutex<Vec<RenderEvent>>>,
}

impl ShellRenderer for CapturingRenderer {
    fn publish(&mut self, event: RenderEvent) {
        self.events.lock().expect("renderer lock").push(event);
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
        load_bashrc: None,
        load_profile: None,
        notification_verbosity: None,
        cage: Default::default(),
        daemon: Default::default(),
    }
}

/// Every Text/SystemInfo/Error block emitted since the last `clear`.
fn blocks_containing(events: &Mutex<Vec<RenderEvent>>, needle: &str) -> Vec<String> {
    events
        .lock()
        .expect("events lock")
        .iter()
        .filter_map(|e| match e {
            RenderEvent::Block(BlockContent::SystemInfo(s))
            | RenderEvent::Block(BlockContent::Text(s))
            | RenderEvent::Block(BlockContent::Error(s)) => s.contains(needle).then(|| s.clone()),
            _ => None,
        })
        .collect()
}

fn errors_containing(events: &Mutex<Vec<RenderEvent>>, needle: &str) -> Vec<String> {
    events
        .lock()
        .expect("events lock")
        .iter()
        .filter_map(|e| match e {
            RenderEvent::Block(BlockContent::Error(s)) => s.contains(needle).then(|| s.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn rfc_show_resolves_project_less() {
    // SAFETY: process-local env mutation, confined to this test binary;
    //         accepted per stdlib's post-1.85 set_var contract.
    unsafe {
        std::env::set_var("EDITOR", "true");
        std::env::set_var("VISUAL", "true");
    }

    let dir = TempDir::new().expect("tmp");
    let renderer = CapturingRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    // RFC create emits SEAL events through the router; without a consumer
    // the channel just buffers, but draining it mirrors the real run.
    let orkia_rx = repl
        .take_orkia_event_rx()
        .expect("orkia_event_rx available pre-run");
    let job_projects: JobProjects = Arc::new(RwLock::new(std::collections::HashMap::new()));
    let _consumer = spawn_consumer(
        orkia_rx,
        SealManager::new(dir.path().to_path_buf()),
        job_projects,
    );

    // Two projects; the same title in both → the same slug `auth-refresh`.
    repl.tick("project create demo \"d\"".into())
        .await
        .expect("create demo");
    repl.tick("project create pay \"p\"".into())
        .await
        .expect("create pay");
    repl.tick(r#"rfc create "auth refresh" --project demo"#.into())
        .await
        .expect("rfc in demo");
    repl.tick(r#"rfc create "auth refresh" --project pay"#.into())
        .await
        .expect("rfc in pay");
    // A slug that lives in exactly one project.
    repl.tick(r#"rfc create "solo idea" --project pay"#.into())
        .await
        .expect("solo rfc");

    // ─── collision: project-less show must REFUSE and name both projects,
    //         and must not render any RFC body.
    events.lock().expect("clear").clear();
    repl.tick("rfc show auth-refresh".into())
        .await
        .expect("tick");
    let ambiguous = errors_containing(&events, "exists in");
    assert_eq!(
        ambiguous.len(),
        1,
        "collision must surface one disambiguation error; got: {ambiguous:?}"
    );
    let msg = &ambiguous[0];
    assert!(
        msg.contains("demo") && msg.contains("pay") && msg.contains("--project"),
        "error must list both projects and the remedy; got: {msg:?}"
    );
    assert!(
        blocks_containing(&events, "## Objective").is_empty(),
        "collision must not render an RFC body"
    );

    // ─── disambiguation: `--project` always works.
    events.lock().expect("clear").clear();
    repl.tick("rfc show auth-refresh --project demo".into())
        .await
        .expect("tick");
    assert!(
        !blocks_containing(&events, "auth refresh").is_empty(),
        "--project must render the RFC"
    );
    assert!(
        errors_containing(&events, "exists in").is_empty(),
        "disambiguated show must not error"
    );

    // ─── unique slug: project-less show resolves outright.
    events.lock().expect("clear").clear();
    repl.tick("rfc show solo-idea".into()).await.expect("tick");
    assert!(
        !blocks_containing(&events, "solo idea").is_empty(),
        "unique slug must resolve project-less"
    );
    assert!(
        errors_containing(&events, "not found").is_empty(),
        "unique slug must not 404"
    );

    // ─── unknown slug: fail-closed with a clear message.
    events.lock().expect("clear").clear();
    repl.tick("rfc show ghost-slug".into()).await.expect("tick");
    assert_eq!(
        errors_containing(&events, "not found in any project").len(),
        1,
        "unknown slug must report not-found"
    );
}

/// The same slug-addressed resolution must hold for the other read-only,
/// slug-named RFC reads: `rfc state`, `rfc lock-status`, and `rfc edit`.
#[tokio::test]
async fn rfc_state_lock_edit_resolve_project_less() {
    // SAFETY: process-local env mutation, confined to this test binary.
    unsafe {
        std::env::set_var("EDITOR", "true");
        std::env::set_var("VISUAL", "true");
    }

    let dir = TempDir::new().expect("tmp");
    let renderer = CapturingRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    let orkia_rx = repl
        .take_orkia_event_rx()
        .expect("orkia_event_rx available pre-run");
    let job_projects: JobProjects = Arc::new(RwLock::new(std::collections::HashMap::new()));
    let _consumer = spawn_consumer(
        orkia_rx,
        SealManager::new(dir.path().to_path_buf()),
        job_projects,
    );

    repl.tick("project create demo \"d\"".into())
        .await
        .expect("create demo");
    repl.tick("project create pay \"p\"".into())
        .await
        .expect("create pay");
    repl.tick(r#"rfc create "auth refresh" --project demo"#.into())
        .await
        .expect("rfc in demo");
    repl.tick(r#"rfc create "auth refresh" --project pay"#.into())
        .await
        .expect("rfc in pay");
    repl.tick(r#"rfc create "solo idea" --project pay"#.into())
        .await
        .expect("solo rfc");

    // Each verb: collision project-less → fail-closed listing both projects;
    // unique slug → resolves project-less without error.
    for verb in ["state", "lock-status", "edit"] {
        events.lock().expect("clear").clear();
        repl.tick(format!("rfc {verb} auth-refresh"))
            .await
            .expect("tick");
        let ambiguous = errors_containing(&events, "exists in");
        assert_eq!(
            ambiguous.len(),
            1,
            "rfc {verb}: collision must surface one disambiguation error; got: {ambiguous:?}"
        );
        assert!(
            ambiguous[0].contains("demo") && ambiguous[0].contains("pay"),
            "rfc {verb}: error must name both projects; got: {:?}",
            ambiguous[0]
        );

        events.lock().expect("clear").clear();
        repl.tick(format!("rfc {verb} solo-idea"))
            .await
            .expect("tick");
        assert!(
            errors_containing(&events, "exists in").is_empty()
                && errors_containing(&events, "not found").is_empty(),
            "rfc {verb}: unique slug must resolve project-less without error"
        );
    }

    // Unknown slug stays fail-closed across the verbs too.
    for verb in ["state", "lock-status", "edit"] {
        events.lock().expect("clear").clear();
        repl.tick(format!("rfc {verb} ghost-slug"))
            .await
            .expect("tick");
        assert_eq!(
            errors_containing(&events, "not found in any project").len(),
            1,
            "rfc {verb}: unknown slug must report not-found"
        );
    }
}

/// An active `rfc cd` scope is an explicit project context (T2.59): it
/// outranks the slug-collision fallback, so a bare colliding slug resolves to
/// the scoped project, the slug itself defaults from the scope, and `rfc exit`
/// restores the project-less collision behaviour.
#[tokio::test]
async fn rfc_cd_scope_outranks_collision() {
    // SAFETY: process-local env mutation, confined to this test binary.
    unsafe {
        std::env::set_var("EDITOR", "true");
        std::env::set_var("VISUAL", "true");
    }

    let dir = TempDir::new().expect("tmp");
    let renderer = CapturingRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    let orkia_rx = repl
        .take_orkia_event_rx()
        .expect("orkia_event_rx available pre-run");
    let job_projects: JobProjects = Arc::new(RwLock::new(std::collections::HashMap::new()));
    let _consumer = spawn_consumer(
        orkia_rx,
        SealManager::new(dir.path().to_path_buf()),
        job_projects,
    );

    repl.tick("project create demo \"d\"".into())
        .await
        .expect("create demo");
    repl.tick("project create pay \"p\"".into())
        .await
        .expect("create pay");
    repl.tick(r#"rfc create "auth refresh" --project demo"#.into())
        .await
        .expect("rfc in demo");
    repl.tick(r#"rfc create "auth refresh" --project pay"#.into())
        .await
        .expect("rfc in pay");

    // Enter the scope of demo's auth-refresh.
    repl.tick("rfc cd auth-refresh --project demo".into())
        .await
        .expect("rfc cd");

    // In scope: a bare colliding slug resolves to demo (scope wins, no error).
    events.lock().expect("clear").clear();
    repl.tick("rfc show auth-refresh".into())
        .await
        .expect("tick");
    assert!(
        errors_containing(&events, "exists in").is_empty(),
        "rfc cd scope must outrank the collision fallback"
    );
    assert!(
        !blocks_containing(&events, "auth refresh").is_empty(),
        "in-scope show must render the scoped RFC"
    );

    // In scope: the slug itself defaults from the scope (bare `rfc state`).
    events.lock().expect("clear").clear();
    repl.tick("rfc state".into()).await.expect("tick");
    assert!(
        !blocks_containing(&events, "rfc:auth-refresh").is_empty(),
        "bare `rfc state` must default slug+project from the scope"
    );

    // After `rfc exit`, the project-less collision behaviour is restored.
    repl.tick("rfc exit".into()).await.expect("rfc exit");
    events.lock().expect("clear").clear();
    repl.tick("rfc show auth-refresh".into())
        .await
        .expect("tick");
    assert_eq!(
        errors_containing(&events, "exists in").len(),
        1,
        "after `rfc exit` the collision fallback applies again"
    );
}

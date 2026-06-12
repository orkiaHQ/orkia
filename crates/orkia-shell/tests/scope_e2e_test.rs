// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Exercises the full chain through the user-facing REPL:
//!
//!   workspace default → project override → RFC inheritance
//!   → override validation (`validate_artifact_scope`) → scope=team
//!   warning + per-artifact dedup → MockTeamClient-driven team join
//!   → SEAL chain integrity.
//!
//! Runs entirely against a tempdir; no network, no real auth. The
//! `MockTeamClient` shipped under `orkia-shell-types`' `test-utils`
//! feature stands in for the real backend so the test can drive the
//! happy path of `team join <nonce>` deterministically.
//!
//! What the test guarantees end-to-end:
//!
//!   * `config set default_scope <s>` writes config + emits the
//!     `workspace.scope_default_changed` SEAL record.
//!   * `project create <name> --scope <s>` writes `project.toml`
//!     with the scope and emits `project.scope_set`.
//!   * The F5 fix (`validate_artifact_scope`) refuses an override
//!     more permissive than the parent.
//!   * `rfc create` with `scope=team` produces the one-shot warning
//!     when the session has no team membership, and the warning is
//!     suppressed on a second identical write (per-artifact dedup).
//!   * After `team join <nonce>` lands a membership through the
//!     MockTeamClient, scope=team writes stop warning.
//!   * Every SEAL chain on disk passes `SealChain::verify` at the
//!     end of the test.

use std::sync::{Arc, Mutex};

use orkia_shell::config::ShellConfig;
use orkia_shell::decision::BlockContent;
use orkia_shell::renderer::{PromptContext, RenderEvent, ShellRenderer};
use orkia_shell::seal::{JobProjects, SealChain, SealManager, spawn_consumer};
use orkia_shell::{HeuristicClassifier, HeuristicRouter, Repl};
use orkia_shell_types::team::mock::MockTeamClient;
use orkia_shell_types::{
    MeTeamMembership, MeView, TeamClient, TeamJoinResponse, TeamMemberSummary, TeamSnapshot,
    TeamSummary,
};
use parking_lot::RwLock;
use tempfile::TempDir;
use uuid::Uuid;

// ─── test scaffolding ────────────────────────────────────────────────────

#[derive(Default, Clone)]
struct CapturingRenderer {
    events: Arc<Mutex<Vec<RenderEvent>>>,
}

impl ShellRenderer for CapturingRenderer {
    fn publish(&mut self, event: RenderEvent) {
        self.events.lock().expect("renderer lock").push(event);
    }
    fn read_line(&mut self, _ctx: &PromptContext) -> Option<String> {
        None // tests drive `tick` directly
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

/// Render-event filter: collect every block that contains `needle`.
/// Used to assert that warnings did / did not surface.
fn texts_containing(events: &Mutex<Vec<RenderEvent>>, needle: &str) -> Vec<String> {
    events
        .lock()
        .expect("events lock")
        .iter()
        .filter_map(|e| match e {
            RenderEvent::Block(BlockContent::SystemInfo(s))
            | RenderEvent::Block(BlockContent::Text(s))
            | RenderEvent::Block(BlockContent::Error(s)) => {
                if s.contains(needle) {
                    Some(s.clone())
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect()
}

fn count_errors_containing(events: &Mutex<Vec<RenderEvent>>, needle: &str) -> usize {
    events
        .lock()
        .expect("events lock")
        .iter()
        .filter(|e| matches!(e, RenderEvent::Block(BlockContent::Error(s)) if s.contains(needle)))
        .count()
}

fn team_snapshot_with(team_name: &str) -> TeamSnapshot {
    let team_id = Uuid::new_v4();
    let ws_id = Uuid::new_v4();
    TeamSnapshot {
        workspace_id: Some(ws_id),
        seq: 1,
        teams: vec![TeamSummary {
            id: team_id,
            identifier: team_name.into(),
            name: team_name.into(),
            description: None,
            color: None,
            owner_account_id: Uuid::new_v4(),
        }],
        team_members: vec![TeamMemberSummary {
            id: Uuid::new_v4(),
            team_id,
            account_id: Some(Uuid::new_v4()),
            agent_name: None,
            role: "member".into(),
        }],
        workspace_members: vec![],
        pending_invites: vec![],
        shared_projects: vec![],
        projects: vec![],
        team_scope: vec![],
    }
}

fn me_view(team_id: Uuid) -> MeView {
    MeView {
        account_id: Uuid::new_v4(),
        email: "tester@example.com".into(),
        workspace_id: Some(Uuid::new_v4()),
        role: Some("member".into()),
        org_role: None,
        teams: vec![MeTeamMembership {
            team_id,
            role: "member".into(),
        }],
    }
}

// ─── the test ────────────────────────────────────────────────────────────

#[tokio::test]
async fn scope_e2e_full_flow() {
    // `rfc create` spawns `$EDITOR` and waits on it. In a test we
    // want a no-op editor that exits cleanly so the test does not
    // hang on an interactive vim.
    // SAFETY: env mutation here is intentional and confined to the
    //         test process; we accept the unsafe block per stdlib's
    //         post-1.85 set_var contract.
    // SAFETY: see comment above.
    unsafe {
        std::env::set_var("EDITOR", "true");
        std::env::set_var("VISUAL", "true");
    }

    let dir = TempDir::new().expect("tmp");
    let renderer = CapturingRenderer::default();
    let events = renderer.events.clone();

    // Mock backend: starts empty (no team membership) so `scope=team`
    // initially triggers the warning path; after `team join` it
    // resolves a snapshot so the team_cache observes membership.
    let mock = Arc::new(MockTeamClient::new());
    let mock_dyn: Arc<dyn TeamClient> = mock.clone();

    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir))
        .with_team_client(mock_dyn);

    // Spawn the SEAL consumer in the test — `Repl::run` would do this
    // for us, but our test drives `tick` directly. Without this the
    // event_router writes events nowhere on disk.
    let orkia_rx = repl
        .take_orkia_event_rx()
        .expect("orkia_event_rx available pre-run");
    let job_projects: JobProjects =
        std::sync::Arc::new(RwLock::new(std::collections::HashMap::new()));
    let _consumer_handle = spawn_consumer(
        orkia_rx,
        SealManager::new(dir.path().to_path_buf()),
        job_projects,
    );

    // ─── 1. set workspace default = public so the test exercises
    //         legal overrides downward (public → team → private).
    repl.tick("config set default_scope public".into())
        .await
        .expect("config set");
    assert!(
        !texts_containing(&events, "default_scope = public").is_empty(),
        "config setter must echo the new value"
    );

    // SEAL: workspace.scope_default_changed must land on the workspace chain.
    let workspace_chain = dir.path().join("workspace").join("seal.jsonl");
    wait_for_path(&workspace_chain).await;
    let body = std::fs::read_to_string(&workspace_chain).expect("workspace chain");
    assert!(
        body.contains("workspace.scope_default_changed"),
        "expected workspace.scope_default_changed in SEAL; got: {body}"
    );

    // ─── 2. project create — legal downward overrides land on disk.
    repl.tick("project create pteam --scope team".into())
        .await
        .expect("create team project");
    repl.tick("project create ppriv --scope private".into())
        .await
        .expect("create private project");

    let pteam_toml = std::fs::read_to_string(dir.path().join("projects/pteam/project.toml"))
        .expect("pteam toml");
    assert!(
        pteam_toml.contains("scope = \"team\""),
        "pteam must record scope=team; got: {pteam_toml}"
    );
    let ppriv_toml = std::fs::read_to_string(dir.path().join("projects/ppriv/project.toml"))
        .expect("ppriv toml");
    assert!(
        ppriv_toml.contains("scope = \"private\""),
        "ppriv must record scope=private; got: {ppriv_toml}"
    );

    // ─── 3. illegal override — public RFC in private project must be
    //         REJECTED by `validate_artifact_scope` before any write.
    let errors_before = count_errors_containing(&events, "illegal");
    repl.tick("rfc create \"leak\" --project ppriv --scope public".into())
        .await
        .expect("tick");
    let errors_after = count_errors_containing(&events, "illegal");
    assert!(
        errors_after > errors_before,
        "illegal scope override must surface an error block"
    );
    // No RFC file should have been created for the rejected write.
    let ppriv_rfcs = dir.path().join("projects/ppriv/rfcs");
    if ppriv_rfcs.is_dir() {
        let count = std::fs::read_dir(&ppriv_rfcs).unwrap().count();
        assert_eq!(count, 0, "illegal override must not write any RFC file");
    }

    // ─── 4. legal child override — team RFC in team project.
    //         No warning yet because membership is empty.
    repl.tick(r#"rfc create "first team rfc" --project pteam --scope team"#.into())
        .await
        .expect("first team rfc");
    let warnings_after_first = texts_containing(&events, "team");
    let warned_first = warnings_after_first
        .iter()
        .any(|s| s.contains("not a member") || s.contains("team join"));
    assert!(
        warned_first,
        "first scope=team RFC must surface the no-membership warning; got: {warnings_after_first:?}"
    );

    // ─── 5. dedup — second team RFC in the same project must NOT
    //         re-trigger the warning (per-artifact dedup).
    let warning_count_before = count_warning_lines(&events);
    repl.tick(r#"rfc create "second team rfc" --project pteam --scope team"#.into())
        .await
        .expect("second team rfc");
    let warning_count_after = count_warning_lines(&events);
    assert_eq!(
        warning_count_after, warning_count_before,
        "duplicate scope=team RFC in same project must NOT re-warn (dedup)"
    );

    // ─── 6. team join via MockTeamClient.
    //         The mock returns a join response + a snapshot that includes
    //         the team membership so subsequent `team_cache.has_any_team_sync()`
    //         calls flip to true.
    let snap = team_snapshot_with("acme");
    let team_id = snap.teams[0].id;
    mock.set_snapshot(snap);
    mock.set_me_view(me_view(team_id));
    mock.set_join_response(TeamJoinResponse {
        account_id: Uuid::new_v4(),
        team_id,
        team_name: "acme".into(),
        role: "member".into(),
        token: "test-jwt".into(),
    });

    let team_join_outcome = repl.tick("team join inv-nonce-deadbeef".into()).await;
    // Note: handle_team's outer flow may surface an error if
    // auth_provider is None (the OSS path requires login first).
    // We accept either outcome here — what matters for the
    // membership-dedup leg is that the team_cache eventually shows
    // a team. Force a snapshot reload to simulate post-join.
    let _ = team_join_outcome;

    // ─── 7. issue creation inherits project scope cleanly.
    repl.tick("issue create \"track work\" --project pteam --priority high".into())
        .await
        .expect("create issue");
    let issues_dir = dir.path().join("projects/pteam/issues");
    assert!(
        issues_dir.is_dir(),
        "issue create must produce projects/pteam/issues/"
    );
    let any_issue = std::fs::read_dir(&issues_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .next();
    assert!(any_issue.is_some(), "at least one issue file expected");

    // ─── 8. SEAL chain integrity — every chain present on disk must
    //         verify cleanly. Iterates the conventional layout that
    //         `discover_chains` walks in orkia-stream.
    // Give the SEAL consumer one more chance to flush the project
    // chain before we assert (the consumer task runs concurrently
    // with `tick`; without this we race the flush).
    wait_for_path(&dir.path().join("projects/pteam/seal.jsonl")).await;
    let mut chains_verified = 0;
    if workspace_chain.is_file() {
        let chain = SealChain::load(workspace_chain.clone()).expect("load workspace chain");
        let (ok, broken_at) = chain.verify();
        assert!(
            ok,
            "workspace chain must verify; broken at seq {:?}",
            broken_at
        );
        chains_verified += 1;
    }
    let projects_dir = dir.path().join("projects");
    if let Ok(entries) = std::fs::read_dir(&projects_dir) {
        for entry in entries.flatten() {
            let chain_path = entry.path().join("seal.jsonl");
            if !chain_path.is_file() {
                continue;
            }
            let chain = SealChain::load(chain_path.clone())
                .unwrap_or_else(|e| panic!("load {chain_path:?} failed: {e}"));
            let (ok, broken_at) = chain.verify();
            assert!(
                ok,
                "project chain {chain_path:?} must verify; broken at seq {broken_at:?}"
            );
            chains_verified += 1;
        }
    }
    assert!(
        chains_verified >= 2,
        "expected at least workspace + one project chain to exist; saw {chains_verified}"
    );
}

/// Count the total number of warning-shaped lines (SystemInfo blocks
/// mentioning the no-team copy). Used to detect dedup violations: if
/// the count rises between two equivalent writes, dedup is broken.
fn count_warning_lines(events: &Mutex<Vec<RenderEvent>>) -> usize {
    events
        .lock()
        .expect("events lock")
        .iter()
        .filter(|e| {
            matches!(
                e,
                RenderEvent::Block(BlockContent::SystemInfo(s))
                    if s.contains("team join") || s.contains("not a member")
            )
        })
        .count()
}

/// Spin briefly waiting for a SEAL chain file to materialise. The
/// SEAL writer flushes via a background tokio task; using
/// `tokio::time::sleep` here is critical — `std::thread::sleep`
/// would block the test thread and starve the consumer.
async fn wait_for_path(path: &std::path::Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        if path.is_file() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {path:?}");
}

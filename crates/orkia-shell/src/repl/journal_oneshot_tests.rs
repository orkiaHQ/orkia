// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Focused tests for the one-shot (`--once`) teardown driven by
//! edge must never strand a dispatched `-c "@agent … --once"` (#8 — a
//! one-shot must reach a terminal state). Covers the two hang paths:
//! a `Stop` whose envelope has no `job_id` (the FinalResponseService
//! bails, so no AFR will ever arrive) and a failure AFR with no
//! `response_path` (extraction failed after retries).

use super::Repl;
use crate::config::ShellConfig;
use crate::job::{JobId, SinkRecipe, SinkTarget};
use crate::journal::{EventType, JournalEnvelope};
use crate::renderer::{PromptContext, RenderEvent, ShellRenderer};
use crate::{HeuristicClassifier, HeuristicRouter};
use orkia_shell_types::job::JobKind;
use tempfile::TempDir;

struct NullRenderer;

impl ShellRenderer for NullRenderer {
    fn publish(&mut self, _event: RenderEvent) {}
    fn read_line(&mut self, _ctx: &PromptContext) -> Option<String> {
        None
    }
}

fn cfg(dir: &TempDir) -> ShellConfig {
    ShellConfig {
        data_dir: dir.path().to_path_buf(),
        agents: vec![],
        agent_commands: std::collections::HashMap::new(),
        native_agents: std::collections::HashSet::new(),
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

/// Spawn a real PTY-backed child and re-label it as a `--once` agent job
/// with a bound Terminal sink, mirroring what `dispatch_agent` produces for
/// a standalone `@faye … --once`.
fn fabricate_once_agent(repl: &mut Repl, dir: &TempDir) -> JobId {
    let argv: Vec<String> = vec!["sleep".into(), "30".into()];
    let id = repl
        .jobs
        .spawn_shell(&argv, vec![], None, "@faye fake --once".into(), dir.path())
        .expect("spawn fake agent");
    let entry = repl.jobs.get_mut(id).expect("entry");
    entry.kind = JobKind::Agent {
        agent_id: uuid::Uuid::nil(),
        agent_name: "faye".into(),
    };
    entry.sink_recipe = Some(SinkRecipe {
        target: SinkTarget::Terminal,
        once: true,
    });
    repl.oneshot_dispatch = true;
    id
}

fn hook_env(event: &str, job_id: Option<u32>) -> JournalEnvelope {
    let mut env = JournalEnvelope::now(EventType::Hook);
    env.event = Some(event.into());
    env.job_id = job_id;
    env.source = Some("claude".into());
    env
}

#[tokio::test]
async fn stop_with_job_id_defers_teardown_to_afr() {
    // The AFR envelope (success or failure) always follows a Stop that
    // carries a job_id, so the Stop itself must NOT tear down — the text
    // is delivered first.
    let dir = TempDir::new().expect("tmp");
    let mut repl = Repl::new(
        NullRenderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg(&dir),
    );
    let id = fabricate_once_agent(&mut repl, &dir);

    repl.route_journal_side_effects(&hook_env("Stop", Some(id.0)));

    assert!(!repl.oneshot_complete, "Stop with job_id must defer to AFR");
    assert!(
        repl.jobs.get(id).is_some_and(|e| e.sink_recipe.is_some()),
        "recipe must survive until the AFR delivers the text"
    );
}

#[tokio::test]
async fn stop_without_job_id_tears_down_immediately() {
    // The FinalResponseService bails on a Stop without job_id — no AFR
    // will ever arrive. Deferring would hang the dispatched command
    // forever; the sole-live-agent fallback must finish the one-shot.
    let dir = TempDir::new().expect("tmp");
    let mut repl = Repl::new(
        NullRenderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg(&dir),
    );
    fabricate_once_agent(&mut repl, &dir);

    repl.route_journal_side_effects(&hook_env("Stop", None));

    assert!(
        repl.oneshot_complete,
        "Stop without job_id can never produce an AFR; teardown must not defer"
    );
}

#[tokio::test]
async fn failure_afr_without_response_path_tears_down() {
    // Extraction failed: the AFR carries no response_path. There is no
    // text to surface, but the `--once` binding is spent — the session
    // must still reach its terminal state instead of hanging.
    let dir = TempDir::new().expect("tmp");
    let mut repl = Repl::new(
        NullRenderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg(&dir),
    );
    let id = fabricate_once_agent(&mut repl, &dir);

    repl.route_journal_side_effects(&hook_env("Stop", Some(id.0)));
    assert!(!repl.oneshot_complete, "precondition: Stop deferred");

    let mut afr = hook_env("AgentFinalResponse", Some(id.0));
    afr.response_preview = Some("<extraction failed: transcript not found>".into());
    repl.route_journal_side_effects(&afr);

    assert!(
        repl.oneshot_complete,
        "failure AFR must still tear the one-shot down"
    );
    assert!(
        repl.jobs.get(id).is_some_and(|e| e.sink_recipe.is_none()),
        "spent binding must be dropped"
    );
}

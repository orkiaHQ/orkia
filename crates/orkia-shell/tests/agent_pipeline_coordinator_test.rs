// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Contract tests for the `AgentPipelineCoordinator` trait.
//!
//! These tests do not exercise a concrete coordinator implementation
//! (the OSS shell ships without one). They verify the dispatcher's
//! contract surface:
//!
//! 1. With no coordinator wired (the default build) `@a | @b`
//!    returns a clear "coordinator required" error.
//! 2. With a coordinator wired, the dispatcher calls
//!    `dispatch(AgentPipelineRequest)` and surfaces the
//!    coordinator's outcome.
//! 3. The coordinator receives the parsed `AgentPipelineStage` list
//!    verbatim (agent names + bodies preserved).

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use orkia_shell::config::ShellConfig;
use orkia_shell::decision::BlockContent;
use orkia_shell::renderer::{PromptContext, RenderEvent, ShellRenderer};
use orkia_shell::{HeuristicClassifier, HeuristicRouter, Repl};
use orkia_shell_types::{
    AgentPipelineCoordinator, AgentPipelineRequest, AgentPipelineStage, PipelineDispatchOutcome,
    PipelineProgressCallback,
};
use std::collections::HashMap;
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

/// A stub coordinator that records every `dispatch` invocation and
/// returns the configured outcome. Lets the test assert on what Solo
/// handed off.
struct StubCoordinator {
    received: Mutex<Vec<AgentPipelineRequest>>,
    outcome: PipelineDispatchOutcome,
}

impl StubCoordinator {
    fn new(outcome: PipelineDispatchOutcome) -> Self {
        Self {
            received: Mutex::new(Vec::new()),
            outcome,
        }
    }
    fn calls(&self) -> Vec<AgentPipelineRequest> {
        self.received.lock().unwrap().clone()
    }
}

impl AgentPipelineCoordinator for StubCoordinator {
    fn dispatch<'a>(
        &'a self,
        request: AgentPipelineRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = PipelineDispatchOutcome> + Send + 'a>> {
        let outcome = self.outcome.clone();
        self.received.lock().unwrap().push(request);
        Box::pin(async move { outcome })
    }
    fn subscribe_progress(&self, _: PipelineProgressCallback) {}
}

// ── Solo default: no coordinator → Team required ────────────────────

#[tokio::test]
async fn solo_default_returns_team_required() {
    let dir = TempDir::new().unwrap();
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    repl.tick("@a do thing | @b review".into()).await.unwrap();
    let text = collect_text(&events.lock().unwrap());
    assert!(
        text.contains("requires Orkia Team"),
        "expected Team-required, got: {text}"
    );
}

// ── With coordinator wired: dispatch reaches it ─────────────────────

#[tokio::test]
async fn coordinator_receives_request_and_outcome_propagates() {
    let dir = TempDir::new().unwrap();
    let stub = Arc::new(StubCoordinator::new(PipelineDispatchOutcome::Launched {
        pipeline_id: "pipe-xyz".into(),
        total_stages: 2,
    }));

    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir))
        .with_pipeline_coordinator(stub.clone() as Arc<dyn AgentPipelineCoordinator>);

    repl.tick("@a do thing | @b review".into()).await.unwrap();

    // Coordinator was called exactly once with two stages.
    let calls = stub.calls();
    assert_eq!(calls.len(), 1, "expected one dispatch call");
    let req = &calls[0];
    match req {
        AgentPipelineRequest::AgentChain { stages } => {
            assert_eq!(stages.len(), 2);
            assert_eq!(stages[0].agent, "a");
            assert_eq!(stages[0].body, "do thing");
            assert_eq!(stages[1].agent, "b");
            assert_eq!(stages[1].body, "review");
        }
        AgentPipelineRequest::ShellThenAgentChain { .. } => {
            panic!("expected AgentChain, got ShellThenAgentChain");
        }
    }

    // Outcome rendered as PipelineStarted.
    let text = collect_text(&events.lock().unwrap());
    assert!(
        text.contains("pipeline 2 stages") || text.contains("pipeline"),
        "expected pipeline-started block, got: {text}"
    );
}

#[tokio::test]
async fn coordinator_refusal_surfaces_to_user() {
    let dir = TempDir::new().unwrap();
    let stub = Arc::new(StubCoordinator::new(PipelineDispatchOutcome::Refused {
        reason: "agent @x not configured".into(),
    }));

    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir))
        .with_pipeline_coordinator(stub.clone() as Arc<dyn AgentPipelineCoordinator>);

    repl.tick("@x | @y".into()).await.unwrap();
    let text = collect_text(&events.lock().unwrap());
    assert!(
        text.contains("pipeline refused"),
        "expected refusal message, got: {text}"
    );
    assert!(
        text.contains("agent @x not configured"),
        "expected refusal reason to surface, got: {text}"
    );
}

#[tokio::test]
async fn three_stage_pipeline_passes_through() {
    let dir = TempDir::new().unwrap();
    let stub = Arc::new(StubCoordinator::new(PipelineDispatchOutcome::Launched {
        pipeline_id: "p1".into(),
        total_stages: 3,
    }));

    let renderer = TestRenderer::default();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir))
        .with_pipeline_coordinator(stub.clone() as Arc<dyn AgentPipelineCoordinator>);

    repl.tick("@a do | @b check | @c ship".into())
        .await
        .unwrap();

    let calls = stub.calls();
    assert_eq!(calls.len(), 1);
    let stages = calls[0].stages();
    assert_eq!(stages.len(), 3);
    let agents: Vec<&str> = stages.iter().map(|s| s.agent.as_str()).collect();
    assert_eq!(agents, vec!["a", "b", "c"]);
    let bodies: Vec<&str> = stages.iter().map(|s| s.body.as_str()).collect();
    assert_eq!(bodies, vec!["do", "check", "ship"]);
}

#[tokio::test]
async fn coordinator_not_called_for_single_agent_dispatch() {
    // `@a do thing` (single agent, no pipe) must never reach the
    // pipeline coordinator — even when one is wired.
    let dir = TempDir::new().unwrap();
    let stub = Arc::new(StubCoordinator::new(PipelineDispatchOutcome::Launched {
        pipeline_id: "p".into(),
        total_stages: 1,
    }));

    let renderer = TestRenderer::default();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir))
        .with_pipeline_coordinator(stub.clone() as Arc<dyn AgentPipelineCoordinator>);

    repl.tick("@a do thing".into()).await.unwrap();

    assert!(
        stub.calls().is_empty(),
        "single-agent dispatch must not reach the pipeline coordinator"
    );
}

// ── AgentPipelineStage clone shape ──────────────────────────────────

#[test]
fn stage_struct_is_cloneable() {
    let s = AgentPipelineStage {
        agent: "x".into(),
        body: "y".into(),
    };
    let c = s.clone();
    assert_eq!(c.agent, "x");
    assert_eq!(c.body, "y");
}

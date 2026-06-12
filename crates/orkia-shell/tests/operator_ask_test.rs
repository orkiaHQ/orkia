// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use orkia_reasoning_core::dto::KnowledgeNode;
use orkia_reasoning_core::enums::{KnowledgeNodeKind, NodeOrigin};
use orkia_reasoning_store::{NodeInsert, ReasoningStore};
use orkia_shell::config::{AgentCommandConfig, ShellConfig};
use orkia_shell::decision::BlockContent;
use orkia_shell::renderer::{PromptContext, RenderEvent, ShellRenderer};
use orkia_shell::{HeuristicClassifier, HeuristicRouter, Repl};
use orkia_shell_types::{AgentInfo, AgentStatus, FinalResponseCallback, FinalResponseEvent};
use tempfile::TempDir;
use uuid::Uuid;

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

fn cfg_with_operator_agent(dir: &TempDir) -> ShellConfig {
    cfg_with_operator_command(dir, "true", Vec::new())
}

fn cfg_with_operator_command(dir: &TempDir, command: &str, args: Vec<String>) -> ShellConfig {
    trust_current_dir(dir);
    let mut agent_commands = HashMap::new();
    agent_commands.insert(
        "operator".into(),
        AgentCommandConfig {
            command: command.into(),
            args: args.clone(),
        },
    );
    ShellConfig {
        agents: vec![AgentInfo {
            id: Uuid::new_v4(),
            name: "operator".into(),
            archetype: "reasoning".into(),
            status: AgentStatus::Idle,
            model: "fake".into(),
            dir: PathBuf::new(),
            description: None,
            command: command.into(),
            args,
            assigned_projects: Vec::new(),
            max_context_tokens: 4000,
        }],
        agent_commands,
        native_agents: Default::default(),
        ..cfg(dir)
    }
}

fn trust_current_dir(dir: &TempDir) {
    let cwd = std::env::current_dir().expect("current dir");
    let canonical = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let raw = serde_json::to_vec_pretty(&vec![canonical.to_string_lossy().to_string()])
        .expect("serialize trusted dir");
    std::fs::write(dir.path().join("trusted_dirs.json"), raw).expect("write trusted dirs");
}

#[derive(Clone)]
struct StaticFinalResponseSource {
    event: Option<FinalResponseEvent>,
    emit_on_subscribe: bool,
}

impl StaticFinalResponseSource {
    fn new(event: Option<FinalResponseEvent>) -> Self {
        Self {
            event,
            emit_on_subscribe: true,
        }
    }

    fn latest_only(event: FinalResponseEvent) -> Self {
        Self {
            event: Some(event),
            emit_on_subscribe: false,
        }
    }
}

impl orkia_shell_types::FinalResponseSource for StaticFinalResponseSource {
    fn subscribe(&self, callback: FinalResponseCallback) {
        if !self.emit_on_subscribe {
            return;
        }
        if let Some(event) = self.event.clone() {
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(20));
                callback(event);
            });
        }
    }

    fn latest_for_job(&self, job_id: u32) -> Option<FinalResponseEvent> {
        self.event
            .as_ref()
            .filter(|event| event.job_id == job_id)
            .cloned()
    }
}

fn event_text(events: &[RenderEvent]) -> String {
    let mut out = String::new();
    for event in events {
        if let RenderEvent::Block(
            BlockContent::Text(text) | BlockContent::SystemInfo(text) | BlockContent::Error(text),
        ) = event
        {
            out.push_str(text);
            out.push('\n');
        }
    }
    out
}

fn seed_reasoning_node(dir: &TempDir, summary: &str, details: Option<&str>) -> String {
    let path = orkia_shell::reasoning_builtins::store_path(dir.path());
    std::fs::create_dir_all(path.parent().expect("reasoning store parent"))
        .expect("create reasoning store parent");
    let store = ReasoningStore::open(&path).expect("open reasoning store");
    let node = KnowledgeNode {
        id: Uuid::new_v4(),
        workspace_id: Uuid::from_u128(1),
        project_id: None,
        rfc_ref: None,
        kind: KnowledgeNodeKind::Decision,
        summary: summary.into(),
        confidence: 0.9,
        origin: NodeOrigin::Cloud,
        created_at: chrono::Utc::now(),
    };
    let citation_id = format!("kg:{}", &node.id.to_string()[..8]);
    store
        .upsert_node(&NodeInsert {
            node: &node,
            details: details.map(str::to_string),
            domain: None,
            context_block: None,
            source_turn_id: None,
            source_session_id: None,
            seal_id: None,
        })
        .expect("upsert reasoning node");
    citation_id
}

fn final_response_event(dir: &TempDir, answer: &str) -> FinalResponseEvent {
    let path = dir.path().join("final-response.md");
    std::fs::write(&path, answer).expect("write final response");
    FinalResponseEvent {
        job_id: 1,
        agent: "operator".into(),
        session_id: Some("session-test".into()),
        response_path: Some(path),
        response_sha256: Some("test".into()),
        response_bytes: answer.len() as u64,
        response_preview: answer.chars().take(280).collect(),
    }
}

#[tokio::test]
async fn operator_ask_json_returns_grounded_citations() {
    let dir = TempDir::new().expect("tmp");
    seed_reasoning_node(
        &dir,
        "Auth uses PKCE for browser session handoff",
        Some("PKCE was selected to avoid long-lived browser bearer secrets."),
    );
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    repl.tick("operator ask why auth uses pkce --json".into())
        .await
        .expect("operator ask");

    let text = event_text(&events.lock().expect("events lock"));
    assert!(text.contains("\"rejected\": false"), "{text}");
    assert!(text.contains("\"source\": \"knowledge_node\""), "{text}");
    assert!(text.contains("[kg:"), "{text}");
}

#[tokio::test]
async fn operator_ask_uses_captured_final_response_when_cited() {
    let dir = TempDir::new().expect("tmp");
    let citation = seed_reasoning_node(
        &dir,
        "Auth uses PKCE for browser session handoff",
        Some("PKCE was selected to avoid long-lived browser bearer secrets."),
    );
    // The citation must precede the sentence punctuation — that's the
    // synthesis_prompt contract verify_citations enforces per segment.
    let answer = format!("Auth uses PKCE for browser handoff [{citation}].");
    let source = StaticFinalResponseSource::new(Some(final_response_event(&dir, &answer)));
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(
        renderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg_with_operator_agent(&dir),
    )
    .with_final_response_source(Arc::new(source));

    repl.tick("operator ask why auth uses pkce --json".into())
        .await
        .expect("operator ask");

    let text = event_text(&events.lock().expect("events lock"));
    assert!(text.contains(&answer), "{text}");
    assert!(text.contains("\"confidence\": 0.8"), "{text}");
    assert!(text.contains("\"rejected\": false"), "{text}");
}

#[tokio::test]
async fn operator_ask_does_not_accept_stale_latest_response() {
    let dir = TempDir::new().expect("tmp");
    seed_reasoning_node(
        &dir,
        "Auth uses PKCE for browser session handoff",
        Some("PKCE was selected to avoid long-lived browser bearer secrets."),
    );
    let stale = final_response_event(&dir, "Stale cached response. [kg:stale]");
    let source = StaticFinalResponseSource::latest_only(stale);
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(
        renderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg_with_operator_agent(&dir),
    )
    .with_final_response_source(Arc::new(source));

    repl.tick("operator ask why auth uses pkce --json --timeout-ms 100".into())
        .await
        .expect("operator ask");

    let text = event_text(&events.lock().expect("events lock"));
    assert!(text.contains("\"rejected\": true"), "{text}");
    assert!(text.contains("final response capture timed out"), "{text}");
    assert!(!text.contains("Stale cached response"), "{text}");
}

#[tokio::test]
async fn operator_ask_captures_response_from_reused_live_agent() {
    let dir = TempDir::new().expect("tmp");
    let citation = seed_reasoning_node(
        &dir,
        "Auth uses PKCE for browser session handoff",
        Some("PKCE was selected to avoid long-lived browser bearer secrets."),
    );
    // Same citation-before-punctuation contract as the cited-answer test.
    let answer = format!("Auth uses PKCE for browser handoff [{citation}].");
    let source = StaticFinalResponseSource::new(Some(final_response_event(&dir, &answer)));
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(
        renderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg_with_operator_command(&dir, "sleep", vec!["5".into()]),
    )
    .with_final_response_source(Arc::new(source));

    repl.tick("@operator keep running".into())
        .await
        .expect("spawn operator");
    repl.tick("operator ask why auth uses pkce --json --timeout-ms 500".into())
        .await
        .expect("operator ask");

    let text = event_text(&events.lock().expect("events lock"));
    assert!(text.contains(&answer), "{text}");
    assert!(text.contains("\"rejected\": false"), "{text}");
}

#[tokio::test]
async fn operator_ask_rejects_uncited_final_response_and_suggests() {
    let dir = TempDir::new().expect("tmp");
    seed_reasoning_node(
        &dir,
        "Auth uses PKCE for browser session handoff",
        Some("PKCE was selected to avoid long-lived browser bearer secrets."),
    );
    let source =
        StaticFinalResponseSource::new(Some(final_response_event(&dir, "Auth uses PKCE.")));
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(
        renderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg_with_operator_agent(&dir),
    )
    .with_final_response_source(Arc::new(source));

    repl.tick("operator ask why auth uses pkce --json".into())
        .await
        .expect("operator ask");

    let text = event_text(&events.lock().expect("events lock"));
    assert!(text.contains("\"rejected\": true"), "{text}");
    assert!(text.contains("answer contained uncited claims"), "{text}");
    assert!(text.contains("[kg:"), "{text}");
}

#[tokio::test]
async fn operator_ask_missing_final_response_falls_back_and_suggests() {
    let dir = TempDir::new().expect("tmp");
    seed_reasoning_node(
        &dir,
        "Auth uses PKCE for browser session handoff",
        Some("PKCE was selected to avoid long-lived browser bearer secrets."),
    );
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(
        renderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg_with_operator_agent(&dir),
    )
    .with_final_response_source(Arc::new(StaticFinalResponseSource::new(None)));

    repl.tick("operator ask why auth uses pkce --json".into())
        .await
        .expect("operator ask");

    let text = event_text(&events.lock().expect("events lock"));
    assert!(text.contains("\"rejected\": true"), "{text}");
    assert!(text.contains("final response capture timed out"), "{text}");
    assert!(text.contains("[kg:"), "{text}");
}

#[tokio::test]
async fn operator_ask_evidence_mode_does_not_wait_for_final_response() {
    let dir = TempDir::new().expect("tmp");
    seed_reasoning_node(
        &dir,
        "Auth uses PKCE for browser session handoff",
        Some("PKCE was selected to avoid long-lived browser bearer secrets."),
    );
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(
        renderer,
        HeuristicClassifier,
        HeuristicRouter,
        cfg_with_operator_agent(&dir),
    )
    .with_final_response_source(Arc::new(StaticFinalResponseSource::new(None)));

    repl.tick("operator ask why auth uses pkce --evidence --json".into())
        .await
        .expect("operator ask");

    let text = event_text(&events.lock().expect("events lock"));
    assert!(text.contains("\"source_ref\""), "{text}");
    assert!(!text.contains("final response capture timed out"), "{text}");
}

#[tokio::test]
async fn operator_ask_unknown_topic_refuses_without_evidence() {
    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    repl.tick("operator ask unknown projection topic --json".into())
        .await
        .expect("operator ask");

    let text = event_text(&events.lock().expect("events lock"));
    assert!(text.contains("\"rejected\": true"), "{text}");
    assert!(text.contains("no grounded evidence found"), "{text}");
}

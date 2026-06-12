// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use orkia_rfc_core::frontmatter::{OperatorConstraints, OperatorFrontmatterBlock};
use orkia_rfc_core::{RfcId, RfcStore};
use orkia_shell_types::JobId;
use tempfile::tempdir;
use tokio::sync::mpsc;

use crate::journal::{EventType, JournalEnvelope};
use crate::operator::{OperatorConfig, spawn};
use crate::protocol::{EventPayload, EventRouter, EventSource, OrkiaEvent};

#[tokio::test]
async fn operator_e2e_emits_hard_drift_to_seal_and_prompt_journal() {
    let dir = tempdir().expect("tempdir");
    write_rfc(
        dir.path(),
        "operator-v1",
        OperatorConstraints {
            allowed_paths: vec!["src/operator/**".into()],
            watch_paths: vec!["src/contracts/**".into()],
            ..Default::default()
        },
        "Do not add shared mutable state.",
    );
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (router, mut seal_rx) = EventRouter::new_with_rx();
    let (journal_tx, mut journal_rx) = mpsc::unbounded_channel::<JournalEnvelope>();
    let handle = spawn(
        input_rx,
        OperatorConfig {
            data_dir: dir.path().to_path_buf(),
            router,
            journal_tx: Some(journal_tx),
        },
    );

    input_tx
        .send(tool_use(
            1,
            "faye",
            "operator-v1",
            "write_file",
            "orkia-private/x.rs",
        ))
        .expect("send tool use");

    let seal_event = recv_custom(&mut seal_rx).await;
    assert!(matches!(
        seal_event.event,
        EventPayload::Custom { ref name, .. } if name == "operator.drift_detected"
    ));
    let journal_event = recv_operator_journal(&mut journal_rx).await;
    assert_eq!(
        journal_event.event.as_deref(),
        Some("operator.drift_detected")
    );
    assert_eq!(journal_event.source.as_deref(), Some("orkia-operator"));
    assert!(
        journal_event
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("outside allowed_paths")
    );
    let suggestion =
        recv_operator_journal_named(&mut journal_rx, "operator.suggestion_created").await;
    assert_eq!(suggestion.extra["kind"], "suggestion");
    assert!(
        suggestion
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("review the RFC constraints")
    );

    drop(input_tx);
    handle.await.expect("operator exits");
}

#[tokio::test]
async fn operator_e2e_detects_cross_session_watch_conflict() {
    let dir = tempdir().expect("tempdir");
    write_rfc(
        dir.path(),
        "writer",
        OperatorConstraints {
            allowed_paths: vec!["src/**".into()],
            ..Default::default()
        },
        "Writer RFC.",
    );
    write_rfc(
        dir.path(),
        "observer",
        OperatorConstraints {
            allowed_paths: vec!["tests/**".into()],
            watch_paths: vec!["src/contracts/**".into()],
            ..Default::default()
        },
        "Observer depends on contract files.",
    );
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (router, mut seal_rx) = EventRouter::new_with_rx();
    let (journal_tx, mut journal_rx) = mpsc::unbounded_channel::<JournalEnvelope>();
    let handle = spawn(
        input_rx,
        OperatorConfig {
            data_dir: dir.path().to_path_buf(),
            router,
            journal_tx: Some(journal_tx),
        },
    );

    input_tx
        .send(tool_use(
            2,
            "sage",
            "observer",
            "read_file",
            "tests/observer.rs",
        ))
        .expect("prime observer");
    input_tx
        .send(tool_use(
            1,
            "faye",
            "writer",
            "write_file",
            "src/contracts/auth.rs",
        ))
        .expect("writer touches watched contract");

    let seal_event = recv_custom_named(&mut seal_rx, "operator.cross_session_conflict").await;
    assert!(matches!(seal_event.event, EventPayload::Custom { .. }));
    let journal_event =
        recv_operator_journal_named(&mut journal_rx, "operator.cross_session_conflict").await;
    assert!(
        journal_event
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("watch_paths")
    );

    drop(input_tx);
    handle.await.expect("operator exits");
}

#[tokio::test]
async fn operator_e2e_detects_cross_session_same_artifact_conflict() {
    let dir = tempdir().expect("tempdir");
    write_rfc(
        dir.path(),
        "writer",
        OperatorConstraints {
            allowed_paths: vec!["src/**".into()],
            ..Default::default()
        },
        "Writer RFC.",
    );
    write_rfc(
        dir.path(),
        "observer",
        OperatorConstraints {
            allowed_paths: vec!["src/**".into()],
            ..Default::default()
        },
        "Observer owns the same module.",
    );
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (router, mut seal_rx) = EventRouter::new_with_rx();
    let (journal_tx, _journal_rx) = mpsc::unbounded_channel::<JournalEnvelope>();
    let handle = spawn(
        input_rx,
        OperatorConfig {
            data_dir: dir.path().to_path_buf(),
            router,
            journal_tx: Some(journal_tx),
        },
    );

    input_tx
        .send(tool_use(2, "sage", "observer", "read_file", "src/api.rs"))
        .expect("prime observer");
    input_tx
        .send(tool_use(1, "faye", "writer", "write_file", "src/api.rs"))
        .expect("writer touches same artifact");

    let seal_event = recv_custom_named(&mut seal_rx, "operator.cross_session_conflict").await;
    let EventPayload::Custom { data, .. } = seal_event.event else {
        panic!("expected custom event");
    };
    assert!(
        data["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("same artifact")
    );

    drop(input_tx);
    handle.await.expect("operator exits");
}

#[tokio::test]
async fn operator_e2e_warns_once_when_rfc_scope_is_missing() {
    let dir = tempdir().expect("tempdir");
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (router, mut seal_rx) = EventRouter::new_with_rx();
    let (journal_tx, mut journal_rx) = mpsc::unbounded_channel::<JournalEnvelope>();
    let handle = spawn(
        input_rx,
        OperatorConfig {
            data_dir: dir.path().to_path_buf(),
            router,
            journal_tx: Some(journal_tx),
        },
    );

    input_tx
        .send(tool_use_without_rfc(1, "faye", "write_file", "src/main.rs"))
        .expect("send first unscoped write");
    input_tx
        .send(tool_use_without_rfc(1, "faye", "write_file", "src/lib.rs"))
        .expect("send second unscoped write");

    let seal_event = recv_custom_named(&mut seal_rx, "operator.drift_detected").await;
    let EventPayload::Custom { data, .. } = seal_event.event else {
        panic!("expected custom event");
    };
    assert_eq!(data["recommended_action"], "attach_rfc_scope");
    let journal_event =
        recv_operator_journal_named(&mut journal_rx, "operator.drift_detected").await;
    assert!(
        journal_event
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("no rfc_id")
    );
    let _suggestion = recv_custom_named(&mut seal_rx, "operator.suggestion_created").await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), seal_rx.recv())
            .await
            .is_err()
    );

    drop(input_tx);
    handle.await.expect("operator exits");
}

#[tokio::test]
async fn operator_e2e_semantic_verdict_cites_option_a_sources() {
    let dir = tempdir().expect("tempdir");
    let project = project_dir(dir.path());
    std::fs::create_dir_all(&project).expect("create project dir");
    std::fs::write(
        project.join("AGENTS.md"),
        "Prefer message passing; do not add Arc<Mutex<T>> shared mutable state.\n",
    )
    .expect("write agents");
    write_rfc(
        dir.path(),
        "semantic",
        OperatorConstraints {
            allowed_paths: vec!["src/**".into()],
            ..Default::default()
        },
        "Architecture: no shared mutable state in the event router.",
    );
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (router, mut seal_rx) = EventRouter::new_with_rx();
    let (journal_tx, _journal_rx) = mpsc::unbounded_channel::<JournalEnvelope>();
    let handle = spawn(
        input_rx,
        OperatorConfig {
            data_dir: dir.path().to_path_buf(),
            router,
            journal_tx: Some(journal_tx),
        },
    );

    let mut event = tool_use(1, "faye", "semantic", "write_file", "src/router.rs");
    if let EventPayload::ToolUse { input_summary, .. } = &mut event.event {
        *input_summary = Some("adds Arc<Mutex<State>> to route events".into());
    }
    input_tx.send(event).expect("send semantic tool use");

    let seal_event = recv_custom_named(&mut seal_rx, "operator.drift_detected").await;
    let EventPayload::Custom { data, .. } = seal_event.event else {
        panic!("expected custom event");
    };
    assert_eq!(data["kind"], "semantic_drift");
    let refs = data["source_refs"].as_array().expect("source refs");
    assert!(refs.iter().any(|r| r["source"] == "AGENTS.md"));
    assert!(refs.iter().any(|r| r["source"] == "RFC body"));
    assert!(refs.iter().any(|r| r["source"] == "recent_tool_history"));

    drop(input_tx);
    handle.await.expect("operator exits");
}

fn write_rfc(data_dir: &std::path::Path, slug: &str, constraints: OperatorConstraints, body: &str) {
    let project = project_dir(data_dir);
    std::fs::create_dir_all(&project).expect("create project dir");
    let store = RfcStore::new(project);
    let id = RfcId::new(slug);
    let mut rec = store.create(&id, Some(slug)).expect("create rfc");
    rec.fm.operator = Some(OperatorFrontmatterBlock {
        constraints: Some(constraints),
    });
    store.save(rec.fm, body.into()).expect("save rfc");
}

fn project_dir(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("projects").join("demo")
}

fn tool_use(job: u32, agent: &str, rfc: &str, tool: &str, target: &str) -> OrkiaEvent {
    OrkiaEvent {
        source: EventSource::Hook {
            provider: "test".into(),
        },
        event: EventPayload::ToolUse {
            tool: tool.into(),
            target: Some(target.into()),
            input_summary: None,
        },
        confidence: 1.0,
        timestamp: chrono::Utc::now(),
        job_id: JobId(job),
        agent_name: agent.into(),
        rfc_id: Some(RfcId::new(rfc)),
    }
}

fn tool_use_without_rfc(job: u32, agent: &str, tool: &str, target: &str) -> OrkiaEvent {
    OrkiaEvent {
        source: EventSource::Hook {
            provider: "test".into(),
        },
        event: EventPayload::ToolUse {
            tool: tool.into(),
            target: Some(target.into()),
            input_summary: None,
        },
        confidence: 1.0,
        timestamp: chrono::Utc::now(),
        job_id: JobId(job),
        agent_name: agent.into(),
        rfc_id: None,
    }
}

async fn recv_custom(rx: &mut mpsc::UnboundedReceiver<OrkiaEvent>) -> OrkiaEvent {
    tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("seal event timeout")
        .expect("seal event")
}

async fn recv_custom_named(rx: &mut mpsc::UnboundedReceiver<OrkiaEvent>, name: &str) -> OrkiaEvent {
    loop {
        let event = recv_custom(rx).await;
        if matches!(event.event, EventPayload::Custom { name: ref n, .. } if n == name) {
            return event;
        }
    }
}

async fn recv_operator_journal(
    rx: &mut mpsc::UnboundedReceiver<JournalEnvelope>,
) -> JournalEnvelope {
    tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("journal event timeout")
        .expect("journal event")
}

async fn recv_operator_journal_named(
    rx: &mut mpsc::UnboundedReceiver<JournalEnvelope>,
    name: &str,
) -> JournalEnvelope {
    loop {
        let event = recv_operator_journal(rx).await;
        if event.event.as_deref() == Some(name) && event.event_type == EventType::Hook {
            return event;
        }
    }
}

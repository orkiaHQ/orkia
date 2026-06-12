// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use super::*;

mod support;

use support::*;

#[test]
fn parse_requires_question() {
    assert!(parse_ask_args(&[]).is_err());
}

#[test]
fn parse_scopes_evidence_and_timeout() {
    let args = [
        "why",
        "auth",
        "--job",
        "42",
        "--rfc",
        "auth-v2",
        "--evidence-agent",
        "@sage",
        "--domain",
        "auth",
        "--cwd",
        "/repo/app",
        "--since",
        "7d",
        "--evidence",
        "--timeout-ms",
        "2500",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    let parsed = parse_ask_args(&args).unwrap();
    assert_eq!(parsed.question, "why auth");
    assert_eq!(parsed.job, Some(42));
    assert_eq!(parsed.rfc.as_deref(), Some("auth-v2"));
    assert_eq!(parsed.evidence_agent.as_deref(), Some("sage"));
    assert_eq!(parsed.domain.as_deref(), Some("auth"));
    assert_eq!(parsed.cwd.as_deref(), Some("/repo/app"));
    assert!(parsed.since.is_some());
    assert!(parsed.evidence_only);
    assert_eq!(parsed.timeout_ms, 2_500);
}

#[test]
fn verifier_rejects_uncited_answer() {
    let citations = vec![citation()];
    assert!(!verify_citations("Auth uses PKCE.", &citations));
    assert!(verify_citations("Auth uses PKCE [kg:abc].", &citations));
    assert!(!verify_citations(
        "Auth uses PKCE. [kg:abc]\nSessions are safer.",
        &citations
    ));
    assert!(verify_answer("Auth uses PKCE [kg:abc].", &citations).accepted);
    assert!(!verify_answer("Auth uses PKCE.", &citations).accepted);
    assert!(!verify_answer("no grounded evidence found", &citations).accepted);
    assert!(verify_answer("no grounded evidence found", &[]).accepted);
}

#[test]
fn extractive_answer_handles_dotted_event_names() {
    let citations = vec![Citation {
        id: "journal:7".into(),
        source: "journal".into(),
        summary: "operator.drift_detected: auth session drift".into(),
        score: 50,
        timestamp: None,
        source_ref: Some("journal://event/7".into()),
        node_id: None,
        seal_id: None,
        job_id: None,
    }];
    let answer = extractive_answer("what happened with operator.drift_detected?", &citations);
    assert!(verify_citations(&answer, &citations), "{answer}");
}

#[test]
fn apply_synthesis_accepts_cited_answer_and_rejects_uncited() {
    let base = ProjectionResponse {
        question: "why auth".into(),
        answer: "extractive [kg:abc]".into(),
        confidence: 0.65,
        citations: vec![citation()],
        rejected: false,
        rejection_reason: None,
    };
    let accepted = apply_synthesis(&base, "Auth uses PKCE [kg:abc].");
    assert!(!accepted.rejected);
    assert_eq!(accepted.answer, "Auth uses PKCE [kg:abc].");
    let rejected = apply_synthesis(&base, "Auth uses PKCE.");
    assert!(rejected.rejected);
    assert_eq!(rejected.answer, base.answer);
}

#[test]
fn projection_and_suggestion_events_are_named() {
    let response = ProjectionResponse {
        question: "why auth".into(),
        answer: "Auth uses PKCE. [kg:abc]".into(),
        confidence: 0.8,
        citations: vec![citation()],
        rejected: false,
        rejection_reason: None,
    };
    let answered = projection_event(&response);
    assert_eq!(
        answered.event.as_deref(),
        Some("operator.projection_answered")
    );
    let mut rejected_response = response.clone();
    rejected_response.rejected = true;
    rejected_response.rejection_reason = Some("final_response_missing".into());
    let rejected = projection_event(&rejected_response);
    assert_eq!(
        rejected.event.as_deref(),
        Some("operator.projection_rejected")
    );
    let suggestion = projection_suggestion_event(
        "why auth",
        "operator",
        "final_response_missing",
        &response.citations,
    );
    assert_eq!(
        suggestion.event.as_deref(),
        Some("operator.suggestion_created")
    );
    assert_eq!(
        suggestion.extra.get("reason").and_then(|v| v.as_str()),
        Some("final_response_missing")
    );
}

#[test]
fn project_finds_reasoning_node_and_cites_it() {
    let dir = tempfile::tempdir().unwrap();
    seed_node(dir.path(), "Auth uses PKCE for device sessions", None, None);
    let journal = JournalStore::new(dir.path());
    let ask = ask("why does auth use pkce");
    let response = project(dir.path(), &journal, &ask);
    assert!(!response.rejected);
    assert!(response.answer.contains("[kg:"));
    assert_eq!(response.citations[0].source, "knowledge_node");
}

#[test]
fn touch_accessed_nodes_bumps_reasoning_store() {
    let dir = tempfile::tempdir().unwrap();
    let id = seed_node(dir.path(), "Auth uses PKCE for device sessions", None, None);
    let journal = JournalStore::new(dir.path());
    let ask = ask("why does auth use pkce");
    let response = project(dir.path(), &journal, &ask);
    assert_eq!(touch_accessed_nodes(dir.path(), &response), 1);
    let store = ReasoningStore::open(&crate::reasoning_builtins::store_path(dir.path())).unwrap();
    assert_eq!(store.access_count(id).unwrap(), Some(1));
}

#[test]
fn project_finds_operator_journal_event() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = JournalStore::new(dir.path());
    let mut env = JournalEnvelope::now(EventType::Hook);
    env.source = Some("orkia-operator".into());
    env.event = Some("operator.drift_detected".into());
    env.message = Some("auth session drift exceeded risk ceiling".into());
    journal.append(&env);
    let ask = ask("what happened with auth drift");
    let response = project(dir.path(), &journal, &ask);
    assert!(!response.rejected);
    assert!(response.answer.contains("[journal:"));
}

#[test]
fn project_finds_seal_journal_event_from_extra() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = JournalStore::new(dir.path());
    let mut env = JournalEnvelope::now(EventType::Hook);
    env.source = Some("orkia".into());
    env.event = Some("agent.complete".into());
    env.message = Some("auth decision completed".into());
    env.job_id = Some(42);
    env.extra.insert(
        "seal_id".into(),
        serde_json::Value::String("seal-auth-1".into()),
    );
    journal.append(&env);
    let ask = ask("what happened with auth seal");
    let response = project(dir.path(), &journal, &ask);
    assert!(!response.rejected);
    assert_eq!(response.citations[0].source, "seal");
    assert_eq!(
        response.citations[0].seal_id.as_deref(),
        Some("seal-auth-1")
    );
    assert_eq!(response.citations[0].job_id, Some(42));
}

#[test]
fn project_filters_journal_by_job_and_rfc() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = JournalStore::new(dir.path());
    let mut wrong = JournalEnvelope::now(EventType::Hook);
    wrong.source = Some("orkia-operator".into());
    wrong.event = Some("operator.drift_detected".into());
    wrong.message = Some("auth drift outside scope".into());
    wrong.job_id = Some(7);
    wrong
        .extra
        .insert("rfc_id".into(), serde_json::json!("other"));
    journal.append(&wrong);
    let mut right = wrong.clone();
    right.job_id = Some(42);
    right
        .extra
        .insert("rfc_id".into(), serde_json::json!("auth-v2"));
    journal.append(&right);
    let mut ask = ask("auth drift scope");
    ask.job = Some(42);
    ask.rfc = Some("auth-v2".into());
    let response = project(dir.path(), &journal, &ask);
    assert_eq!(response.citations.len(), 1);
    assert_eq!(response.citations[0].job_id, Some(42));
}

#[test]
fn project_filters_reasoning_by_rfc() {
    let dir = tempfile::tempdir().unwrap();
    seed_node_with_rfc(dir.path(), "Auth uses PKCE", "auth-v2");
    seed_node_with_rfc(dir.path(), "Auth uses password grant", "legacy");
    let journal = JournalStore::new(dir.path());
    let mut ask = ask("auth");
    ask.rfc = Some("auth-v2".into());
    let response = project(dir.path(), &journal, &ask);
    assert_eq!(response.citations.len(), 1);
    assert!(
        response.citations[0]
            .source_ref
            .as_deref()
            .unwrap()
            .contains("auth-v2")
    );
}

#[test]
fn project_scores_reasoning_by_domain_and_recency() {
    let dir = tempfile::tempdir().unwrap();
    seed_node_with_domain(dir.path(), "Auth uses PKCE", "auth");
    seed_node_with_domain(dir.path(), "Auth uses PKCE", "billing");
    let journal = JournalStore::new(dir.path());
    let mut ask = ask("why auth uses pkce");
    ask.domain = Some("auth".into());
    let response = project(dir.path(), &journal, &ask);
    assert_eq!(response.citations[0].source, "knowledge_node");
    assert!(
        response.citations[0].score > response.citations[1].score,
        "{:?}",
        response.citations
    );
}

#[test]
fn project_scores_reasoning_by_agent() {
    let dir = tempfile::tempdir().unwrap();
    seed_node_with_agent(dir.path(), "Auth uses PKCE", "other");
    seed_node_with_agent(dir.path(), "Auth uses PKCE", "sage");
    let journal = JournalStore::new(dir.path());
    let mut ask = ask("why auth uses pkce");
    ask.evidence_agent = Some("sage".into());
    let response = project(dir.path(), &journal, &ask);
    assert_eq!(response.citations[0].source, "knowledge_node");
    assert!(
        response.citations[0].score > response.citations[1].score,
        "{:?}",
        response.citations
    );
}

#[test]
fn project_scores_journal_by_agent_cwd_domain_and_recency() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = JournalStore::new(dir.path());
    let mut weak = JournalEnvelope::now(EventType::Hook);
    weak.agent = Some("other".into());
    weak.event = Some("Decision".into());
    weak.message = Some("auth session changed".into());
    weak.extra.insert("cwd".into(), serde_json::json!("/tmp"));
    weak.extra
        .insert("domain".into(), serde_json::json!("billing"));
    journal.append(&weak);
    let mut strong = weak.clone();
    strong.agent = Some("sage".into());
    strong
        .extra
        .insert("cwd".into(), serde_json::json!("/repo/app"));
    strong
        .extra
        .insert("domain".into(), serde_json::json!("auth"));
    journal.append(&strong);
    let mut ask = ask("why auth session changed");
    ask.evidence_agent = Some("sage".into());
    ask.cwd = Some("/repo/app".into());
    ask.domain = Some("auth".into());
    let response = project(dir.path(), &journal, &ask);
    assert_eq!(response.citations[0].source, "journal");
    assert!(response.citations[0].score > response.citations[1].score);
}

#[test]
fn render_evidence_includes_source_refs_and_skips_answer() {
    let response = ProjectionResponse {
        question: "why auth".into(),
        answer: "Auth uses PKCE. [kg:abc]".into(),
        confidence: 0.8,
        citations: vec![citation()],
        rejected: false,
        rejection_reason: None,
    };
    let blocks = render(&response, false, true);
    let text = format!("{blocks:?}");
    assert!(text.contains("operator evidence"));
    assert!(text.contains("ref=kg://node/abc"));
    assert!(!text.contains("confidence: 0.80"));
}

#[test]
fn synthesis_prompt_contains_evidence_and_rules() {
    let citations = vec![citation()];
    let prompt = synthesis_prompt("why auth", &citations);
    assert!(prompt.contains("Question:\nwhy auth"));
    assert!(prompt.contains("EvidencePack"));
    assert!(prompt.contains("Every factual sentence MUST include"));
    assert!(prompt.contains("kg:abc"));
}

#[test]
fn project_rejects_when_no_grounding_exists() {
    let dir = tempfile::tempdir().unwrap();
    let journal = JournalStore::new(dir.path());
    let ask = ask("unknown topic");
    let response = project(dir.path(), &journal, &ask);
    assert!(response.rejected);
    assert_eq!(response.answer, "no grounded evidence found");
}

#[test]
fn search_matches_details_and_context_block() {
    let dir = tempfile::tempdir().unwrap();
    seed_node(
        dir.path(),
        "A decision",
        Some("structured sessions rationale"),
        Some("auth_context_marker"),
    );
    let path = crate::reasoning_builtins::store_path(dir.path());
    let store = ReasoningStore::open(&path).unwrap();
    assert_eq!(store.search_nodes_text("rationale", 5).unwrap().len(), 1);
    assert_eq!(
        store
            .search_nodes_text("auth_context_marker", 5)
            .unwrap()
            .len(),
        1
    );
}

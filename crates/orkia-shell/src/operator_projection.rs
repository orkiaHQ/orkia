// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use std::path::Path;

use chrono::{DateTime, Utc};
use orkia_reasoning_core::compile_context_block;
use orkia_reasoning_store::{KnowledgeNodeSearchHit, ReasoningStore};
use orkia_shell_types::{BlockContent, EventType, JournalEnvelope};
use serde::Serialize;
use uuid::Uuid;

use crate::journal::{JournalFilter, JournalStore};

mod args;
mod ranking;

pub use args::{AskArgs, parse_ask_args};

#[derive(Debug, Clone, Serialize)]
pub struct ProjectionResponse {
    pub question: String,
    pub answer: String,
    pub confidence: f32,
    pub citations: Vec<Citation>,
    pub rejected: bool,
    pub rejection_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verification {
    pub accepted: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Citation {
    pub id: String,
    pub source: String,
    pub summary: String,
    pub score: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seal_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<u32>,
}

pub fn project(data_dir: &Path, journal: &JournalStore, ask: &AskArgs) -> ProjectionResponse {
    let citations = collect_evidence(data_dir, journal, ask);
    if citations.is_empty() {
        return ProjectionResponse {
            question: ask.question.clone(),
            answer: "no grounded evidence found".into(),
            confidence: 0.0,
            citations,
            rejected: true,
            rejection_reason: Some("no grounded evidence found".into()),
        };
    }
    let answer = extractive_answer(&ask.question, &citations);
    let rejected = !verify_citations(&answer, &citations);
    ProjectionResponse {
        question: ask.question.clone(),
        answer,
        confidence: if rejected { 0.0 } else { 0.65 },
        citations,
        rejected,
        rejection_reason: rejected.then(|| "answer contained uncited claims".into()),
    }
}

pub fn apply_synthesis(base: &ProjectionResponse, answer: &str) -> ProjectionResponse {
    let verification = verify_answer(answer, &base.citations);
    if verification.accepted {
        let mut accepted = base.clone();
        accepted.answer = answer.trim().to_string();
        accepted.confidence = 0.8;
        accepted.rejected = false;
        accepted.rejection_reason = None;
        return accepted;
    }
    let mut rejected = base.clone();
    rejected.rejected = true;
    rejected.confidence = 0.0;
    rejected.rejection_reason = verification.reason;
    rejected
}

pub fn verify_answer(answer: &str, citations: &[Citation]) -> Verification {
    let trimmed = answer.trim();
    if trimmed.is_empty() {
        return Verification {
            accepted: false,
            reason: Some("answer was empty".into()),
        };
    }
    if trimmed == "no grounded evidence found" {
        return Verification {
            accepted: citations.is_empty(),
            reason: (!citations.is_empty()).then(|| "answer refused despite evidence".into()),
        };
    }
    if citations.is_empty() {
        return Verification {
            accepted: false,
            reason: Some("answer had no evidence pack".into()),
        };
    }
    if verify_citations(trimmed, citations) {
        return Verification {
            accepted: true,
            reason: None,
        };
    }
    Verification {
        accepted: false,
        reason: Some("answer contained uncited claims".into()),
    }
}

pub fn synthesis_prompt(question: &str, citations: &[Citation]) -> String {
    let evidence = serde_json::to_string_pretty(citations).unwrap_or_else(|_| "[]".to_string());
    format!(
        "You are the Orkia operator projection agent.\n\
         Answer the user question using ONLY the evidence below.\n\
         Every factual sentence MUST include at least one citation id before sentence punctuation, \
         for example: Auth uses PKCE [kg:abc123].\n\
         If the evidence is insufficient, answer exactly: no grounded evidence found.\n\n\
         Question:\n{question}\n\n\
         EvidencePack:\n{evidence}"
    )
}

pub fn render(response: &ProjectionResponse, json: bool, evidence_only: bool) -> Vec<BlockContent> {
    if json {
        return vec![BlockContent::Text(
            serde_json::to_string_pretty(response).unwrap_or_else(|_| "{}".into()),
        )];
    }
    let title = if evidence_only {
        " operator evidence"
    } else {
        " operator projection"
    };
    let mut out = vec![BlockContent::SystemInfo(title.into())];
    out.push(BlockContent::Text(format!("  q: {}", response.question)));
    if !evidence_only {
        out.push(BlockContent::Text(format!("  {}", response.answer)));
        out.push(BlockContent::Text(format!(
            "  confidence: {:.2}",
            response.confidence
        )));
    }
    if response.rejected
        && let Some(reason) = &response.rejection_reason
    {
        out.push(BlockContent::Text(format!("  rejected: {reason}")));
    }
    if !response.citations.is_empty() {
        out.push(BlockContent::Text("  citations:".into()));
        for citation in &response.citations {
            let source_ref = citation
                .source_ref
                .as_deref()
                .map(|value| format!(" ref={value}"))
                .unwrap_or_default();
            out.push(BlockContent::Text(format!(
                "    [{}] score={} source={}{} {}",
                citation.id, citation.score, citation.source, source_ref, citation.summary
            )));
        }
    }
    out
}

fn collect_evidence(data_dir: &Path, journal: &JournalStore, ask: &AskArgs) -> Vec<Citation> {
    let mut citations = Vec::new();
    citations.extend(reasoning_citations(data_dir, ask));
    citations.extend(journal_citations(journal, ask));
    citations.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| b.id.cmp(&a.id)));
    citations.truncate(ask.last);
    citations
}

fn reasoning_citations(data_dir: &Path, ask: &AskArgs) -> Vec<Citation> {
    let path = crate::reasoning_builtins::store_path(data_dir);
    let Ok(store) = ReasoningStore::open(&path) else {
        return Vec::new();
    };
    let hits = if let Some(rfc) = &ask.rfc {
        store
            .node_hits_for_rfc(rfc, ask.last.saturating_mul(3).max(10))
            .unwrap_or_default()
    } else {
        store
            .search_node_hits(&ask.question, ask.last.saturating_mul(4).max(16))
            .unwrap_or_default()
    };
    if hits.is_empty() {
        return Vec::new();
    }
    hits.into_iter()
        .filter(|hit| ask.since.is_none_or(|since| hit.node.created_at >= since))
        .filter(|hit| {
            ask.rfc.as_ref().is_none_or(|rfc| {
                hit.node.rfc_ref.as_ref().map(|r| r.rfc_id.as_str()) == Some(rfc.as_str())
            })
        })
        .map(|hit| node_citation(hit, ask))
        .collect()
}

fn node_citation(hit: KnowledgeNodeSearchHit, ask: &AskArgs) -> Citation {
    let node = hit.node;
    let id = node.id.to_string();
    let source_ref = if let Some(rfc) = &node.rfc_ref {
        format!("kg://rfc/{}/node/{id}", rfc.rfc_id)
    } else {
        format!("kg://node/{id}")
    };
    let score_text = format!(
        "{} {} {}",
        node.summary,
        hit.context_block.as_deref().unwrap_or_default(),
        hit.domain.as_deref().unwrap_or_default(),
    );
    Citation {
        id: format!("kg:{}", &id[..8]),
        source: "knowledge_node".into(),
        summary: compile_context_block(&node),
        score: 30
            + ranking::term_score(&ask.question, &score_text)
            + ranking::semantic_score(&ask.question, &score_text)
            + ranking::recency_score(node.created_at)
            + ranking::agent_score(
                ask.evidence_agent.as_deref().or(ask.agent.as_deref()),
                hit.agent_name.as_deref(),
            )
            + ranking::domain_score(ask.domain.as_deref(), hit.domain.as_deref(), &ask.question)
            + (node.confidence * 10.0) as i32,
        timestamp: Some(node.created_at.to_rfc3339()),
        source_ref: Some(source_ref),
        node_id: Some(id),
        seal_id: hit.seal_id,
        job_id: None,
    }
}

fn journal_citations(journal: &JournalStore, ask: &AskArgs) -> Vec<Citation> {
    let terms = ranking::tokens(&ask.question);
    if terms.is_empty() {
        return Vec::new();
    }
    let filter = JournalFilter {
        job_id: ask.job,
        since: ask.since,
        last_n: Some(200),
        ..Default::default()
    };
    journal
        .query_indexed(&filter)
        .into_iter()
        .filter(|(_, env)| rfc_matches(env, ask.rfc.as_deref()))
        .filter_map(|(idx, env)| journal_match(idx, env, &terms, ask))
        .collect()
}

fn journal_match(
    index: usize,
    env: &JournalEnvelope,
    terms: &[String],
    ask: &AskArgs,
) -> Option<Citation> {
    let extra = extra_text(env);
    let haystack = [
        env.source.as_deref(),
        env.event.as_deref(),
        env.message.as_deref(),
        env.tool.as_deref(),
        env.target.as_deref(),
        env.description.as_deref(),
        Some(extra.as_str()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
    .to_lowercase();
    if !terms.iter().any(|term| haystack.contains(term)) {
        return None;
    }
    let source = env.source.as_deref().unwrap_or("journal");
    let event = env.event.as_deref().unwrap_or("-");
    let message = env.message.as_deref().unwrap_or("-");
    let seal_ref = env
        .extra
        .get("seal_id")
        .or_else(|| env.extra.get("seal_path"))
        .and_then(|value| value.as_str());
    let citation_source = if seal_ref.is_some() { "seal" } else { source };
    let citation_id = if seal_ref.is_some() {
        format!("seal:{index}")
    } else {
        format!("journal:{index}")
    };
    let seal_suffix = seal_ref
        .map(|seal| format!(" seal={seal}"))
        .unwrap_or_default();
    Some(Citation {
        id: citation_id,
        source: citation_source.into(),
        summary: format!("{event}: {message}{seal_suffix}"),
        score: journal_score(citation_source, event, message, &extra, env, terms, ask),
        timestamp: Some(env.timestamp.clone()),
        source_ref: Some(format!("journal://event/{index}")),
        node_id: None,
        seal_id: seal_ref.map(str::to_string),
        job_id: env.job_id,
    })
}

fn rfc_matches(env: &JournalEnvelope, rfc: Option<&str>) -> bool {
    let Some(rfc) = rfc else {
        return true;
    };
    env.extra
        .get("rfc_id")
        .or_else(|| env.extra.get("rfc"))
        .and_then(|value| value.as_str())
        == Some(rfc)
}

fn journal_score(
    source: &str,
    event: &str,
    message: &str,
    extra: &str,
    env: &JournalEnvelope,
    terms: &[String],
    ask: &AskArgs,
) -> i32 {
    let base = match source {
        "seal" => 28,
        "orkia-operator" => 24,
        _ => 12,
    };
    let haystack = format!("{event} {message} {extra}").to_lowercase();
    let timestamp = DateTime::parse_from_rfc3339(&env.timestamp)
        .map(|ts| ranking::recency_score(ts.with_timezone(&Utc)))
        .unwrap_or_default();
    base + timestamp
        + ranking::agent_score(
            ask.evidence_agent.as_deref().or(ask.agent.as_deref()),
            env.agent.as_deref().or_else(|| extra_string(env, "agent")),
        )
        + ranking::cwd_score(ask.cwd.as_deref(), extra_string(env, "cwd"))
        + ranking::domain_score(
            ask.domain.as_deref(),
            extra_string(env, "domain"),
            &ask.question,
        )
        + ranking::semantic_score(&ask.question, &haystack)
        + terms
            .iter()
            .filter(|term| haystack.contains(term.as_str()))
            .count() as i32
            * 3
}

fn extra_text(env: &JournalEnvelope) -> String {
    env.extra
        .iter()
        .filter_map(|(key, value)| match value {
            serde_json::Value::String(s) => Some(format!("{key} {s}")),
            serde_json::Value::Number(n) => Some(format!("{key} {n}")),
            serde_json::Value::Bool(b) => Some(format!("{key} {b}")),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn extra_string<'a>(env: &'a JournalEnvelope, key: &str) -> Option<&'a str> {
    env.extra.get(key).and_then(|value| value.as_str())
}

fn extractive_answer(question: &str, citations: &[Citation]) -> String {
    let evidence = citations
        .iter()
        .take(3)
        .map(|c| format!("[{}] {}", c.id, citation_safe_text(&c.summary)))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "[{}] Evidence-backed projection for `{}`.\n{}",
        citations[0].id,
        citation_safe_text(question.trim()),
        evidence
    )
}

fn citation_safe_text(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if matches!(ch, '.' | '?' | '!') {
                ';'
            } else {
                ch
            }
        })
        .collect()
}

fn verify_citations(answer: &str, citations: &[Citation]) -> bool {
    let citation_ids: Vec<String> = citations.iter().map(|c| format!("[{}]", c.id)).collect();
    factual_segments(answer).into_iter().all(|segment| {
        !segment.chars().any(char::is_alphabetic)
            || citation_ids.iter().any(|id| segment.contains(id))
    })
}

fn factual_segments(answer: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in answer.chars() {
        current.push(ch);
        if matches!(ch, '.' | '?' | '!' | '\n') {
            let trimmed = current.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
            current.clear();
        }
    }
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
    }
    out
}

pub fn projection_event(response: &ProjectionResponse) -> JournalEnvelope {
    let mut env = JournalEnvelope::now(EventType::Hook);
    env.source = Some("orkia-operator".into());
    env.event = Some(if response.rejected {
        "operator.projection_rejected".into()
    } else {
        "operator.projection_answered".into()
    });
    env.message = Some(response.answer.clone());
    env.extra.insert(
        "question".into(),
        serde_json::Value::String(response.question.clone()),
    );
    env.extra
        .insert("confidence".into(), serde_json::json!(response.confidence));
    env.extra
        .insert("rejected".into(), serde_json::json!(response.rejected));
    env.extra.insert(
        "citations".into(),
        serde_json::to_value(&response.citations).unwrap_or(serde_json::Value::Null),
    );
    if let Some(reason) = &response.rejection_reason {
        env.extra.insert(
            "rejection_reason".into(),
            serde_json::Value::String(reason.clone()),
        );
    }
    env
}

pub fn projection_suggestion_event(
    question: &str,
    agent: &str,
    reason: &str,
    citations: &[Citation],
) -> JournalEnvelope {
    let mut env = JournalEnvelope::now(EventType::Hook);
    env.source = Some("orkia-operator".into());
    env.event = Some("operator.suggestion_created".into());
    env.message = Some(format!(
        "operator ask could not capture a grounded final response from @{agent}; showing extractive evidence pack instead."
    ));
    env.extra.insert(
        "question".into(),
        serde_json::Value::String(question.to_string()),
    );
    env.extra
        .insert("agent".into(), serde_json::Value::String(agent.to_string()));
    env.extra.insert(
        "reason".into(),
        serde_json::Value::String(reason.to_string()),
    );
    env.extra.insert(
        "recommended_action".into(),
        serde_json::Value::String(
            "inspect the extractive citations or attach to the synthesizer agent".into(),
        ),
    );
    env.extra.insert(
        "citations".into(),
        serde_json::to_value(citations).unwrap_or(serde_json::Value::Null),
    );
    env
}

pub fn knowledge_access_event(response: &ProjectionResponse) -> Option<JournalEnvelope> {
    let ids: Vec<String> = response
        .citations
        .iter()
        .filter_map(|citation| citation.node_id.clone())
        .collect();
    (!ids.is_empty()).then(|| JournalEnvelope::knowledge_access(None, &ids))
}

pub fn touch_accessed_nodes(data_dir: &Path, response: &ProjectionResponse) -> usize {
    let ids: Vec<Uuid> = response
        .citations
        .iter()
        .filter_map(|citation| citation.node_id.as_deref())
        .filter_map(|id| id.parse().ok())
        .collect();
    if ids.is_empty() {
        return 0;
    }
    let path = crate::reasoning_builtins::store_path(data_dir);
    let Ok(store) = ReasoningStore::open(&path) else {
        return 0;
    };
    store.touch_nodes_accessed(&ids).unwrap_or(0)
}

#[cfg(test)]
mod tests;

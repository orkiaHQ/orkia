// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

impl Repl {
    pub(crate) async fn handle_operator(&mut self, args: &[String]) -> Outcome {
        let sub = args.first().map(String::as_str).unwrap_or("status");
        let blocks = match sub {
            "status" => operator_status(&self.journal_store),
            "events" => operator_events(&self.journal_store, args.get(1..).unwrap_or_default()),
            "suggestions" => {
                operator_suggestions(&self.journal_store, args.get(1..).unwrap_or_default())
            }
            "watch" => operator_watch(&self.config.data_dir),
            "explain" => {
                let Some(id) = args.get(1) else {
                    return Outcome::Error("operator explain: missing event id".into());
                };
                operator_explain(&self.journal_store, id)
            }
            "open" | "source" => operator_open(
                &self.config.data_dir,
                &self.journal_store,
                args.get(1..).unwrap_or_default(),
            ),
            "ask" => {
                return self
                    .handle_operator_ask(args.get(1..).unwrap_or_default())
                    .await;
            }
            "-h" | "--help" | "help" => operator_help(),
            other => {
                return Outcome::UsageError(format!(
                    "operator: unknown subcommand `{other}`\n{}",
                    operator_usage()
                ));
            }
        };
        Outcome::BuiltinOutput { blocks }
    }

    async fn handle_operator_ask(&mut self, args: &[String]) -> Outcome {
        let mut ask = match crate::operator_projection::parse_ask_args(args) {
            Ok(ask) => ask,
            Err(err) => return Outcome::UsageError(err),
        };
        if ask.cwd.is_none() {
            ask.cwd = self
                .cwd_cache
                .as_ref()
                .map(|path| path.to_string_lossy().to_string())
                .or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .map(|path| path.to_string_lossy().to_string())
                });
        }
        let mut response =
            crate::operator_projection::project(&self.config.data_dir, &self.journal_store, &ask);
        if let Some(env) = crate::operator_projection::knowledge_access_event(&response) {
            self.emit_journal(env);
        }
        if self.intelligence.is_none() {
            let _ =
                crate::operator_projection::touch_accessed_nodes(&self.config.data_dir, &response);
        }
        let projection_agent = match projection_agent(&self.agents, ask.agent.as_deref()) {
            Ok(agent) => agent,
            Err(err) => return Outcome::Error(err),
        };
        let mut suggestion = None;
        if !ask.evidence_only
            && !response.citations.is_empty()
            && let Some(agent) = projection_agent
        {
            let prompt =
                crate::operator_projection::synthesis_prompt(&ask.question, &response.citations);
            let capture = self.prepare_projection_capture(&agent);
            match self.dispatch_agent(Some(&agent), &prompt, None).await {
                Outcome::JobSpawned { job_id, .. } => {
                    match self
                        .wait_for_projection_final_response(
                            capture.for_job(job_id),
                            Duration::from_millis(ask.timeout_ms),
                        )
                        .await
                    {
                        Some(event) => match final_response_text(&event) {
                            Some(answer) => {
                                let synthesized =
                                    crate::operator_projection::apply_synthesis(&response, &answer);
                                if synthesized.rejected {
                                    suggestion = Some(
                                        crate::operator_projection::projection_suggestion_event(
                                            &ask.question,
                                            &agent,
                                            "synthesis_rejected",
                                            &response.citations,
                                        ),
                                    );
                                }
                                response = synthesized;
                            }
                            None => {
                                response.rejected = true;
                                response.confidence = 0.0;
                                response.rejection_reason =
                                    Some("final response was empty or unavailable".into());
                                suggestion =
                                    Some(crate::operator_projection::projection_suggestion_event(
                                        &ask.question,
                                        &agent,
                                        "final_response_empty",
                                        &response.citations,
                                    ));
                            }
                        },
                        None => {
                            response.rejected = true;
                            response.confidence = 0.0;
                            response.rejection_reason =
                                Some("final response capture timed out".into());
                            suggestion =
                                Some(crate::operator_projection::projection_suggestion_event(
                                    &ask.question,
                                    &agent,
                                    "final_response_missing",
                                    &response.citations,
                                ));
                        }
                    }
                }
                outcome => {
                    if let Some(existing_job) = capture.existing_job {
                        match self
                            .wait_for_projection_final_response(
                                capture.for_job(existing_job),
                                Duration::from_millis(ask.timeout_ms),
                            )
                            .await
                        {
                            Some(event) => match final_response_text(&event) {
                                Some(answer) => {
                                    let synthesized = crate::operator_projection::apply_synthesis(
                                        &response, &answer,
                                    );
                                    if synthesized.rejected {
                                        suggestion = Some(
                                            crate::operator_projection::projection_suggestion_event(
                                                &ask.question,
                                                &agent,
                                                "synthesis_rejected",
                                                &response.citations,
                                            ),
                                        );
                                    }
                                    response = synthesized;
                                }
                                None => {
                                    response.rejected = true;
                                    response.confidence = 0.0;
                                    response.rejection_reason =
                                        Some("final response was empty or unavailable".into());
                                    suggestion = Some(
                                        crate::operator_projection::projection_suggestion_event(
                                            &ask.question,
                                            &agent,
                                            "final_response_empty",
                                            &response.citations,
                                        ),
                                    );
                                }
                            },
                            None => {
                                response.rejected = true;
                                response.confidence = 0.0;
                                response.rejection_reason =
                                    Some("final response capture timed out".into());
                                suggestion =
                                    Some(crate::operator_projection::projection_suggestion_event(
                                        &ask.question,
                                        &agent,
                                        "final_response_missing",
                                        &response.citations,
                                    ));
                            }
                        }
                    } else {
                        response.rejected = true;
                        response.confidence = 0.0;
                        response.rejection_reason =
                            Some(projection_dispatch_failure(&outcome).into());
                        suggestion = Some(crate::operator_projection::projection_suggestion_event(
                            &ask.question,
                            &agent,
                            "final_response_unavailable",
                            &response.citations,
                        ));
                    }
                }
            }
        }
        self.emit_journal(crate::operator_projection::projection_event(&response));
        if let Some(env) = suggestion {
            self.emit_journal(env);
        }
        Outcome::BuiltinOutput {
            blocks: crate::operator_projection::render(&response, ask.json, ask.evidence_only),
        }
    }
}

fn projection_dispatch_failure(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Error(_) => "synthesis agent dispatch failed",
        _ => "synthesis agent did not spawn or reuse a TUI job",
    }
}

fn final_response_text(event: &orkia_shell_types::FinalResponseEvent) -> Option<String> {
    let path = event.response_path.as_ref()?;
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn operator_status(store: &JournalStore) -> Vec<BlockContent> {
    let events = operator_envelopes(store);
    let drift = events
        .iter()
        .filter(|e| e.event.as_deref() == Some("operator.drift_detected"))
        .count();
    let cross = events
        .iter()
        .filter(|e| e.event.as_deref() == Some("operator.cross_session_conflict"))
        .count();
    let suggestions = events
        .iter()
        .filter(|e| e.event.as_deref() == Some("operator.suggestion_created"))
        .count();
    let projections_answered = events
        .iter()
        .filter(|e| e.event.as_deref() == Some("operator.projection_answered"))
        .count();
    let projections_rejected = events
        .iter()
        .filter(|e| e.event.as_deref() == Some("operator.projection_rejected"))
        .count();
    let mut blocks = vec![BlockContent::SystemInfo(" operator status".into())];
    blocks.push(BlockContent::Text(
        "  mode: notify + inert suggestions".into(),
    ));
    blocks.push(BlockContent::Text(format!(
        "  events: {total}",
        total = events.len()
    )));
    blocks.push(BlockContent::Text(format!("  drift: {drift}")));
    blocks.push(BlockContent::Text(format!("  cross-session: {cross}")));
    blocks.push(BlockContent::Text(format!("  suggestions: {suggestions}")));
    blocks.push(BlockContent::Text(format!(
        "  projections: answered={projections_answered} rejected={projections_rejected}"
    )));
    if let Some(last) = events.last() {
        blocks.push(BlockContent::Text(format!(
            "  last: {} {}",
            last.event.as_deref().unwrap_or("-"),
            last.message.as_deref().unwrap_or("-")
        )));
    }
    blocks
}

fn operator_events(store: &JournalStore, args: &[String]) -> Vec<BlockContent> {
    let last = parse_last(args).unwrap_or(20);
    let events = operator_envelopes(store);
    let skip = events.len().saturating_sub(last);
    let mut blocks = vec![BlockContent::SystemInfo(format!(
        " operator events (last {last})"
    ))];
    for (idx, env) in events.iter().enumerate().skip(skip) {
        blocks.push(BlockContent::Text(format_operator_row(idx + 1, env)));
    }
    if blocks.len() == 1 {
        blocks.push(BlockContent::Text("  no operator events".into()));
    }
    blocks
}

fn operator_suggestions(store: &JournalStore, args: &[String]) -> Vec<BlockContent> {
    let last = parse_last(args).unwrap_or(20);
    let events: Vec<_> = operator_envelopes(store)
        .into_iter()
        .filter(|e| e.event.as_deref() == Some("operator.suggestion_created"))
        .collect();
    let skip = events.len().saturating_sub(last);
    let mut blocks = vec![BlockContent::SystemInfo(format!(
        " operator suggestions (last {last})"
    ))];
    for (idx, env) in events.iter().enumerate().skip(skip) {
        blocks.push(BlockContent::Text(format_operator_row(idx + 1, env)));
    }
    if blocks.len() == 1 {
        blocks.push(BlockContent::Text("  no operator suggestions".into()));
    }
    blocks
}

fn operator_explain(store: &JournalStore, id: &str) -> Vec<BlockContent> {
    let Some(index) = parse_event_index(id) else {
        return vec![BlockContent::Error(format!(
            "operator explain: invalid id `{id}` (expected op-N or N)"
        ))];
    };
    let events = operator_envelopes(store);
    let Some(env) = events.get(index.saturating_sub(1)) else {
        return vec![BlockContent::Error(format!(
            "operator explain: no event `{id}`"
        ))];
    };
    let mut blocks = vec![BlockContent::SystemInfo(format!(
        " operator event op-{index}"
    ))];
    blocks.push(BlockContent::Text(format!(
        "  type: {}",
        env.event.as_deref().unwrap_or("-")
    )));
    blocks.push(BlockContent::Text(format!(
        "  job: {}",
        env.job_id
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".into())
    )));
    blocks.push(BlockContent::Text(format!(
        "  agent: {}",
        env.agent.as_deref().unwrap_or("-")
    )));
    blocks.push(BlockContent::Text(format!(
        "  reason: {}",
        env.message.as_deref().unwrap_or("-")
    )));
    for key in [
        "kind",
        "severity",
        "confidence",
        "rfc_id",
        "recommended_action",
        "observed_action",
        "source_refs",
        "question",
        "rejection_reason",
        "citations",
    ] {
        if let Some(value) = env.extra.get(key) {
            blocks.push(BlockContent::Text(format!("  {key}: {value}")));
        }
    }
    blocks
}

fn operator_watch(data_dir: &Path) -> Vec<BlockContent> {
    let mut blocks = vec![BlockContent::SystemInfo(" operator watch paths".into())];
    for item in accepted_watch_paths(data_dir) {
        blocks.push(BlockContent::Text(item));
    }
    if blocks.len() == 1 {
        blocks.push(BlockContent::Text("  no accepted watch_paths".into()));
    }
    blocks
}

fn operator_help() -> Vec<BlockContent> {
    vec![
        BlockContent::SystemInfo(" operator".into()),
        BlockContent::Text(operator_usage().into()),
    ]
}

fn operator_usage() -> &'static str {
    "usage: orkia operator status|events|suggestions|watch|explain <op-N>|open <source-ref> [--json]|ask <question> [--agent @name] [--evidence-agent @name] [--domain NAME] [--cwd PATH] [--last N] [--job ID] [--rfc ID] [--since NN[smhd]|RFC3339] [--evidence] [--timeout-ms N] [--json]"
}

fn operator_open(data_dir: &Path, journal: &JournalStore, args: &[String]) -> Vec<BlockContent> {
    let mut json = false;
    let mut source_ref = None;
    for arg in args {
        if arg == "--json" {
            json = true;
        } else {
            source_ref = Some(arg.as_str());
        }
    }
    let Some(source_ref) = source_ref else {
        return vec![BlockContent::Error(
            "operator open: missing source reference".into(),
        )];
    };
    if json {
        return vec![BlockContent::Text(
            serde_json::to_string_pretty(&crate::operator_sources::resolve_json(
                data_dir, journal, source_ref,
            ))
            .unwrap_or_else(|_| "{}".into()),
        )];
    }
    crate::operator_sources::resolve(data_dir, journal, source_ref)
}

fn projection_agent(
    agents: &[AgentInfo],
    requested: Option<&str>,
) -> Result<Option<String>, String> {
    if let Some(name) = requested {
        if agents.iter().any(|agent| agent.name == name) {
            return Ok(Some(name.to_string()));
        }
        return Err(format!("operator ask: no such agent @{name}"));
    }
    Ok(default_projection_agent(agents))
}

fn default_projection_agent(agents: &[AgentInfo]) -> Option<String> {
    agents
        .iter()
        .find(|agent| agent.name == "operator")
        .or_else(|| {
            agents.iter().find(|agent| {
                matches!(
                    agent.archetype.as_str(),
                    "review" | "reasoning" | "reviewer"
                )
            })
        })
        .map(|agent| agent.name.clone())
}

fn operator_envelopes(store: &JournalStore) -> Vec<&JournalEnvelope> {
    let filter = crate::journal::JournalFilter {
        event_type: Some(EventType::Hook),
        source: Some("orkia-operator".into()),
        ..Default::default()
    };
    store.query(&filter)
}

fn format_operator_row(index: usize, env: &JournalEnvelope) -> String {
    let event = env.event.as_deref().unwrap_or("-");
    let severity = env
        .extra
        .get("severity")
        .and_then(|v| v.as_str())
        .unwrap_or("-");
    let reason = env.message.as_deref().unwrap_or("-");
    let job = env
        .job_id
        .map(|n| n.to_string())
        .unwrap_or_else(|| "-".into());
    let rfc = env
        .extra
        .get("rfc_id")
        .and_then(|v| v.as_str())
        .unwrap_or("-");
    format!("  op-{index} {event} severity={severity} job={job} rfc={rfc} {reason}")
}

fn parse_last(args: &[String]) -> Option<usize> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--last" {
            return iter.next().and_then(|v| v.parse().ok());
        }
    }
    args.first().and_then(|v| v.parse().ok())
}

fn parse_event_index(raw: &str) -> Option<usize> {
    let n = raw.strip_prefix("op-").unwrap_or(raw).parse().ok()?;
    (n > 0).then_some(n)
}

fn accepted_watch_paths(data_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let projects = data_dir.join("projects");
    let Ok(entries) = std::fs::read_dir(projects) else {
        return out;
    };
    for entry in entries.flatten() {
        collect_project_watch_paths(&entry.path(), &mut out);
    }
    out.sort();
    out
}

fn collect_project_watch_paths(project_dir: &Path, out: &mut Vec<String>) {
    let rfcs = project_dir.join("rfcs");
    let Ok(entries) = std::fs::read_dir(rfcs) else {
        return;
    };
    let store = orkia_rfc_core::RfcStore::new(project_dir);
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let id = orkia_rfc_core::RfcId::new(stem);
        let Ok(record) = store.load(&id) else {
            continue;
        };
        let Some(constraints) = record.fm.operator.and_then(|o| o.constraints) else {
            continue;
        };
        for watch in constraints.watch_paths {
            out.push(format!("  {id}: {watch}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_status_counts_projection_events() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut store = JournalStore::new(dir.path());
        store.append(&operator_event("operator.projection_answered"));
        store.append(&operator_event("operator.projection_rejected"));
        store.append(&operator_event("operator.suggestion_created"));

        let text = operator_status(&store)
            .into_iter()
            .map(|block| format!("{block:?}"))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("suggestions: 1"), "{text}");
        assert!(
            text.contains("projections: answered=1 rejected=1"),
            "{text}"
        );
    }

    #[test]
    fn operator_open_json_returns_structured_source() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut store = JournalStore::new(dir.path());
        store.append(&operator_event("operator.projection_answered"));
        let args = vec!["journal://event/1".to_string(), "--json".to_string()];
        let blocks = operator_open(dir.path(), &store, &args);
        let Some(BlockContent::Text(text)) = blocks.first() else {
            panic!("expected json text block");
        };
        let value: serde_json::Value = serde_json::from_str(text).expect("json");
        assert_eq!(
            value.get("kind").and_then(serde_json::Value::as_str),
            Some("journal_event")
        );
        assert_eq!(
            value.get("source_ref").and_then(serde_json::Value::as_str),
            Some("journal://event/1")
        );
    }

    fn operator_event(event: &str) -> JournalEnvelope {
        let mut env = JournalEnvelope::now(EventType::Hook);
        env.source = Some("orkia-operator".into());
        env.event = Some(event.into());
        env.message = Some("test".into());
        env
    }
}

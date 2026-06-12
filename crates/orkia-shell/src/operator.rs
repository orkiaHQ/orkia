// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Orkia Sys operator: notify-only drift detection over the structured agent
//! event stream. The actor owns its session graph and emits SEAL records through
//! `EventRouter`; it never reads PTY screen bytes and never blocks the REPL.

use std::collections::HashMap;
use std::path::PathBuf;

use orkia_rfc_core::RfcId;
use orkia_rfc_core::frontmatter::OperatorConstraints;
use orkia_shell_types::JobId;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::journal::{EventType, JournalEnvelope};
use crate::protocol::{EventPayload, EventRouter, OrkiaEvent};

pub struct OperatorConfig {
    pub data_dir: PathBuf,
    pub router: EventRouter,
    pub journal_tx: Option<UnboundedSender<JournalEnvelope>>,
}

pub fn spawn(
    rx: UnboundedReceiver<OrkiaEvent>,
    cfg: OperatorConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move { OperatorSupervisor::new(cfg).run(rx).await })
}

struct OperatorSupervisor {
    data_dir: PathBuf,
    router: EventRouter,
    journal_tx: Option<UnboundedSender<JournalEnvelope>>,
    graph: OperatorGraph,
}

#[derive(Default)]
struct OperatorGraph {
    sessions: HashMap<JobId, SessionState>,
}

#[derive(Default)]
struct SessionState {
    agent: String,
    rfc_id: Option<RfcId>,
    constraints: Option<OperatorConstraints>,
    touched: Vec<String>,
    missing_scope_reported: bool,
}

#[derive(Clone)]
struct Verdict {
    event_type: &'static str,
    kind: &'static str,
    severity: &'static str,
    confidence: f32,
    job_id: JobId,
    agent: String,
    rfc_id: Option<RfcId>,
    reason: String,
    recommended_action: &'static str,
    observed_action: serde_json::Value,
    source_refs: Vec<serde_json::Value>,
}

struct VerdictDraft {
    event_type: &'static str,
    kind: &'static str,
    severity: &'static str,
    confidence: f32,
    reason: String,
    recommended_action: &'static str,
    observed_action: serde_json::Value,
    source_refs: Vec<serde_json::Value>,
}

impl OperatorSupervisor {
    fn new(cfg: OperatorConfig) -> Self {
        Self {
            data_dir: cfg.data_dir,
            router: cfg.router,
            journal_tx: cfg.journal_tx,
            graph: OperatorGraph::default(),
        }
    }

    async fn run(mut self, mut rx: UnboundedReceiver<OrkiaEvent>) {
        while let Some(event) = rx.recv().await {
            for verdict in self.evaluate(event) {
                self.emit(verdict);
            }
        }
    }

    fn evaluate(&mut self, event: OrkiaEvent) -> Vec<Verdict> {
        if let EventPayload::Custom { name, .. } = &event.event
            && name.starts_with("operator.")
        {
            return Vec::new();
        }

        self.refresh_session(&event);
        let mut verdicts = Vec::new();
        match &event.event {
            EventPayload::ToolUse {
                tool,
                target,
                input_summary,
            } => {
                verdicts.extend(self.evaluate_tool(&event, tool, target.as_deref(), input_summary));
            }
            EventPayload::PermissionRequest {
                tool,
                description,
                risk,
            } => {
                verdicts.extend(self.evaluate_permission(
                    &event,
                    tool.as_deref(),
                    description,
                    risk,
                ));
            }
            _ => {}
        }
        verdicts
    }

    fn refresh_session(&mut self, event: &OrkiaEvent) {
        let rfc_id = event.rfc_id.clone();
        let constraints = rfc_id
            .as_ref()
            .and_then(|id| crate::operator_context::load_constraints(&self.data_dir, id));
        let entry = self.graph.sessions.entry(event.job_id).or_default();
        if !event.agent_name.is_empty() {
            entry.agent = event.agent_name.clone();
        }
        if rfc_id.is_some() {
            entry.rfc_id = rfc_id;
        }
        if constraints.is_some() {
            entry.constraints = constraints;
        }
    }

    fn evaluate_tool(
        &mut self,
        event: &OrkiaEvent,
        tool: &str,
        target: Option<&str>,
        input_summary: &Option<String>,
    ) -> Vec<Verdict> {
        let mut out = Vec::new();
        let target = target.unwrap_or_default();
        if !target.is_empty()
            && let Some(state) = self.graph.sessions.get_mut(&event.job_id)
        {
            state.touched.push(target.to_string());
        }

        if let Some(v) = self.missing_scope_verdict(event, tool, target) {
            out.push(v);
        }
        if let Some(v) = self.hard_path_verdict(event, tool, target) {
            out.push(v);
        }
        if let Some(v) = self.semantic_verdict(event, tool, target, input_summary) {
            out.push(v);
        }
        out.extend(self.cross_session_verdicts(event, tool, target));
        out
    }

    fn evaluate_permission(
        &mut self,
        event: &OrkiaEvent,
        tool: Option<&str>,
        description: &str,
        risk: &Option<String>,
    ) -> Vec<Verdict> {
        let mut out = Vec::new();
        if let Some(v) =
            self.missing_scope_verdict(event, tool.unwrap_or("permission"), description)
        {
            out.push(v);
        }
        let Some(state) = self.graph.sessions.get(&event.job_id) else {
            return out;
        };
        let Some(c) = state.constraints.as_ref() else {
            return out;
        };
        if let Some(risk) = risk
            && let Some(ceiling) = c.risk_ceiling.as_deref()
            && risk_rank(risk) > risk_rank(ceiling)
        {
            out.push(self.verdict(
                event,
                VerdictDraft {
                    event_type: "operator.drift_detected",
                    kind: "hard_violation",
                    severity: "warning",
                    confidence: 1.0,
                    reason: format!(
                        "permission risk '{risk}' exceeds RFC risk_ceiling '{ceiling}'"
                    ),
                    recommended_action: "review_scope",
                    observed_action: serde_json::json!({
                        "tool": tool,
                        "risk": risk,
                        "description": description
                    }),
                    source_refs: Vec::new(),
                },
            ));
        }
        if c.forbidden_commands
            .iter()
            .any(|p| pattern_match(p, description))
        {
            out.push(self.verdict(
                event,
                VerdictDraft {
                    event_type: "operator.drift_detected",
                    kind: "hard_violation",
                    severity: "critical",
                    confidence: 1.0,
                    reason: "permission request matches forbidden command constraint".into(),
                    recommended_action: "review_scope",
                    observed_action: serde_json::json!({
                        "tool": tool,
                        "description": description
                    }),
                    source_refs: Vec::new(),
                },
            ));
        }
        out
    }

    fn missing_scope_verdict(
        &mut self,
        event: &OrkiaEvent,
        tool: &str,
        target: &str,
    ) -> Option<Verdict> {
        if event.rfc_id.is_some() || event.job_id.0 == 0 {
            return None;
        }
        if !is_write_tool(tool) && !is_permission_like(tool) {
            return None;
        }
        let state = self.graph.sessions.entry(event.job_id).or_default();
        if state.missing_scope_reported {
            return None;
        }
        state.missing_scope_reported = true;
        Some(self.verdict(
            event,
            VerdictDraft {
                event_type: "operator.drift_detected",
                kind: "hard_violation",
                severity: "warning",
                confidence: 1.0,
                reason:
                    "operator cannot ground this agent action because the job has no rfc_id".into(),
                recommended_action: "attach_rfc_scope",
                observed_action: serde_json::json!({"tool": tool, "target": target}),
                source_refs: Vec::new(),
            },
        ))
    }

    fn hard_path_verdict(&self, event: &OrkiaEvent, tool: &str, target: &str) -> Option<Verdict> {
        if target.is_empty() {
            return None;
        }
        let state = self.graph.sessions.get(&event.job_id)?;
        let constraints = state.constraints.as_ref()?;
        if constraints
            .forbidden_paths
            .iter()
            .any(|p| pattern_match(p, target))
        {
            return Some(self.verdict(
                event,
                VerdictDraft {
                    event_type: "operator.drift_detected",
                    kind: "hard_violation",
                    severity: "critical",
                    confidence: 1.0,
                    reason: format!("tool target '{target}' matches forbidden_paths"),
                    recommended_action: "review_scope",
                    observed_action: serde_json::json!({"tool": tool, "target": target}),
                    source_refs: Vec::new(),
                },
            ));
        }
        if is_write_tool(tool)
            && !constraints.allowed_paths.is_empty()
            && !constraints
                .allowed_paths
                .iter()
                .any(|p| pattern_match(p, target))
        {
            return Some(self.verdict(
                event,
                VerdictDraft {
                    event_type: "operator.drift_detected",
                    kind: "hard_violation",
                    severity: "warning",
                    confidence: 1.0,
                    reason: format!("write target '{target}' is outside allowed_paths"),
                    recommended_action: "review_scope",
                    observed_action: serde_json::json!({"tool": tool, "target": target}),
                    source_refs: Vec::new(),
                },
            ));
        }
        None
    }

    fn cross_session_verdicts(&self, event: &OrkiaEvent, tool: &str, target: &str) -> Vec<Verdict> {
        if target.is_empty() || !is_write_tool(tool) {
            return Vec::new();
        }
        self.graph
            .sessions
            .iter()
            .filter(|(job, _)| **job != event.job_id)
            .filter_map(|(job, state)| {
                let constraints = state.constraints.as_ref()?;
                let watched = constraints
                    .watch_paths
                    .iter()
                    .any(|p| pattern_match(p, target));
                let exact_prior_touch = state.touched.iter().any(|p| p == target);
                let contract_area = is_contract_path(target)
                    && constraints
                        .allowed_paths
                        .iter()
                        .any(|p| pattern_match(p, target));
                let reason = if watched {
                    Some("watch_paths")
                } else if exact_prior_touch {
                    Some("same artifact touched by another session")
                } else if contract_area {
                    Some("contract-like path overlaps another RFC scope")
                } else {
                    None
                }?;
                Some({
                    self.verdict(
                        event,
                        VerdictDraft {
                            event_type: "operator.cross_session_conflict",
                            kind: "cross_session_conflict",
                            severity: "warning",
                            confidence: 1.0,
                            reason: format!(
                                "write target '{target}' intersects {reason} for job {} ({})",
                                job.0, state.agent
                            ),
                            recommended_action: "notify_affected_session",
                            observed_action: serde_json::json!({
                                "tool": tool,
                                "target": target,
                                "affected_job_id": job.0,
                                "affected_agent": state.agent,
                                "affected_rfc_id": state.rfc_id.as_ref().map(|r| r.as_str()),
                            }),
                            source_refs: Vec::new(),
                        },
                    )
                })
            })
            .collect()
    }

    fn semantic_verdict(
        &self,
        event: &OrkiaEvent,
        tool: &str,
        target: &str,
        input_summary: &Option<String>,
    ) -> Option<Verdict> {
        let observed = format!("{tool} {target} {}", input_summary.as_deref().unwrap_or(""));
        let rule = semantic_rule(&observed)?;
        let rfc_id = event.rfc_id.as_ref()?;
        let ctx = crate::operator_context::load(&self.data_dir, rfc_id, Some(target));
        let mut source_refs = Vec::new();
        if let Some(agent_rule) = ctx.agents_rule.filter(|s| rule.matches_source(s)) {
            source_refs.push(serde_json::json!({
                "source": "AGENTS.md",
                "kind": rule.kind,
                "text": agent_rule,
            }));
        }
        if let Some(body) = ctx.rfc_body.as_deref()
            && let Some(line) = body.lines().find(|l| rule.matches_source(l))
        {
            source_refs.push(serde_json::json!({
                "source": "RFC body",
                "kind": rule.kind,
                "text": line.trim(),
            }));
        }
        source_refs.extend(ctx.kg_refs.into_iter().map(|n| {
            serde_json::json!({
                "source": "knowledge_node",
                "id": n.id,
                "summary": n.summary,
            })
        }));
        if let Some(state) = self.graph.sessions.get(&event.job_id) {
            let recent: Vec<_> = state.touched.iter().rev().take(5).cloned().collect();
            if !recent.is_empty() {
                source_refs.push(serde_json::json!({
                    "source": "recent_tool_history",
                    "targets": recent,
                }));
            }
        }
        if source_refs.is_empty() {
            return None;
        }
        Some(self.verdict(
            event,
            VerdictDraft {
                event_type: "operator.drift_detected",
                kind: "semantic_drift",
                severity: "warning",
                confidence: rule.confidence,
                reason: rule.reason.into(),
                recommended_action: "ask_human",
                observed_action: serde_json::json!({"tool": tool, "target": target}),
                source_refs,
            },
        ))
    }

    fn verdict(&self, event: &OrkiaEvent, draft: VerdictDraft) -> Verdict {
        Verdict {
            event_type: draft.event_type,
            kind: draft.kind,
            severity: draft.severity,
            confidence: draft.confidence,
            job_id: event.job_id,
            agent: event.agent_name.clone(),
            rfc_id: event.rfc_id.clone(),
            reason: draft.reason,
            recommended_action: draft.recommended_action,
            observed_action: draft.observed_action,
            source_refs: draft.source_refs,
        }
    }

    fn emit(&self, verdict: Verdict) {
        self.emit_verdict(&verdict);
        if let Some(suggestion) = suggestion_for(&verdict) {
            self.emit_verdict(&suggestion);
        }
    }

    fn emit_verdict(&self, verdict: &Verdict) {
        let rfc_id = verdict.rfc_id.clone();
        let detail = serde_json::json!({
            "kind": verdict.kind,
            "severity": verdict.severity,
            "confidence": verdict.confidence,
            "job_id": verdict.job_id.0,
            "agent": verdict.agent,
            "rfc_id": verdict.rfc_id.as_ref().map(|r| r.as_str()),
            "source_refs": verdict.source_refs,
            "observed_action": verdict.observed_action,
            "reason": verdict.reason,
            "recommended_action": verdict.recommended_action,
        });
        self.router.on_custom_with_rfc(
            verdict.job_id,
            &verdict.agent,
            verdict.event_type,
            detail,
            rfc_id,
        );
        if let Some(tx) = &self.journal_tx {
            let mut env = JournalEnvelope::now(EventType::Hook);
            env.source = Some("orkia-operator".into());
            env.event = Some(verdict.event_type.into());
            env.job_id = Some(verdict.job_id.0);
            if !verdict.agent.is_empty() {
                env.agent = Some(verdict.agent.clone());
            }
            env.message = Some(verdict.reason.clone());
            env.extra.insert(
                "kind".into(),
                serde_json::Value::String(verdict.kind.into()),
            );
            env.extra.insert(
                "severity".into(),
                serde_json::Value::String(verdict.severity.into()),
            );
            env.extra
                .insert("confidence".into(), serde_json::json!(verdict.confidence));
            env.extra.insert(
                "recommended_action".into(),
                serde_json::Value::String(verdict.recommended_action.into()),
            );
            env.extra
                .insert("observed_action".into(), verdict.observed_action.clone());
            env.extra.insert(
                "source_refs".into(),
                serde_json::Value::Array(verdict.source_refs.clone()),
            );
            if let Some(rfc_id) = &verdict.rfc_id {
                env.extra.insert(
                    "rfc_id".into(),
                    serde_json::Value::String(rfc_id.as_str().to_string()),
                );
            }
            let _ = tx.send(env);
        }
    }
}

fn is_write_tool(tool: &str) -> bool {
    matches!(
        tool.to_ascii_lowercase().as_str(),
        "write"
            | "writefile"
            | "write_file"
            | "edit"
            | "multiedit"
            | "multi_edit"
            | "apply_patch"
            | "bash"
            | "exec_command"
            | "shell"
            | "shell_command"
    )
}

fn is_permission_like(tool: &str) -> bool {
    matches!(
        tool.to_ascii_lowercase().as_str(),
        "permission" | "permissionrequest" | "permission_request"
    )
}

fn is_contract_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    ["contract", "schema", "protocol", "api", "migration"]
        .iter()
        .any(|needle| lower.contains(needle))
}

struct SemanticRule {
    kind: &'static str,
    reason: &'static str,
    confidence: f32,
    observed_needles: &'static [&'static str],
    source_needles: &'static [&'static str],
}

impl SemanticRule {
    fn matches_observed(&self, observed: &str) -> bool {
        let lower = observed.to_ascii_lowercase();
        self.observed_needles
            .iter()
            .any(|needle| lower.contains(needle))
    }

    fn matches_source(&self, source: &str) -> bool {
        let lower = source.to_ascii_lowercase();
        self.source_needles
            .iter()
            .any(|needle| lower.contains(needle))
    }
}

fn semantic_rule(observed: &str) -> Option<&'static SemanticRule> {
    semantic_rules()
        .iter()
        .find(|rule| rule.matches_observed(observed))
}

fn semantic_rules() -> &'static [SemanticRule] {
    &[
        SemanticRule {
            kind: "architecture_rule",
            reason: "change appears to introduce shared mutable state in an operator-visible path",
            confidence: 0.72,
            observed_needles: &["arc<mutex", "mutex<", "shared mutable"],
            source_needles: &["arc<mutex", "mutex", "shared mutable", "message passing"],
        },
        SemanticRule {
            kind: "test_policy",
            reason: "change appears to skip or weaken expected test coverage",
            confidence: 0.66,
            observed_needles: &["skip test", "remove test", "delete test", "no test"],
            source_needles: &["test", "coverage", "acceptance"],
        },
        SemanticRule {
            kind: "security_policy",
            reason: "change appears to touch secret or credential handling",
            confidence: 0.7,
            observed_needles: &["secret", "credential", ".env", "token", "api key"],
            source_needles: &["secret", "credential", ".env", "token", "untrusted"],
        },
        SemanticRule {
            kind: "reliability_policy",
            reason: "change appears to add panic-prone handling in shell infrastructure",
            confidence: 0.64,
            observed_needles: &["unwrap()", "expect(", "panic!"],
            source_needles: &["unwrap", "expect", "panic", "never panic", "untrusted"],
        },
    ]
}

fn suggestion_for(verdict: &Verdict) -> Option<Verdict> {
    if verdict.event_type == "operator.suggestion_created" {
        return None;
    }
    let suggestion_text = match verdict.recommended_action {
        "attach_rfc_scope" => {
            "Attach this job to an RFC scope before continuing, or accept that operator drift checks will stay advisory-only."
        }
        "review_scope" => {
            "Pause the agent and review the RFC constraints. If the action is legitimate, update accepted constraints with `orkia rfc constraints accept`; otherwise steer the agent back inside scope."
        }
        "notify_affected_session" => {
            "Notify the affected session owner before continuing; reconcile the shared contract or watched artifact in the RFCs."
        }
        "ask_human" => {
            "Ask for human confirmation before proceeding; cite the RFC/AGENTS/KG sources shown in the operator event."
        }
        _ => return None,
    };
    let mut observed = verdict.observed_action.clone();
    if let Some(obj) = observed.as_object_mut() {
        obj.insert(
            "suggestion_text".into(),
            serde_json::Value::String(suggestion_text.into()),
        );
        obj.insert(
            "source_event_type".into(),
            serde_json::Value::String(verdict.event_type.into()),
        );
    }
    Some(Verdict {
        event_type: "operator.suggestion_created",
        kind: "suggestion",
        severity: "info",
        confidence: verdict.confidence,
        job_id: verdict.job_id,
        agent: verdict.agent.clone(),
        rfc_id: verdict.rfc_id.clone(),
        reason: suggestion_text.into(),
        recommended_action: "human_approval_required",
        observed_action: observed,
        source_refs: verdict.source_refs.clone(),
    })
}

fn risk_rank(risk: &str) -> u8 {
    match risk.to_ascii_lowercase().as_str() {
        "low" => 1,
        "medium" => 2,
        "high" => 3,
        "critical" => 4,
        _ => 0,
    }
}

fn pattern_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    if pattern == "*" || pattern == "**" {
        return true;
    }
    wildcard_match(pattern.as_bytes(), value.as_bytes())
}

fn wildcard_match(pattern: &[u8], value: &[u8]) -> bool {
    let (mut p, mut v) = (0, 0);
    let (mut star, mut match_at) = (None, 0);
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == value[v]) {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            match_at = v;
            p += 1;
        } else if let Some(s) = star {
            p = s + 1;
            match_at += 1;
            v = match_at;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_constraints_match_paths() {
        assert!(pattern_match("orkia/**", "orkia/crates/x.rs"));
        assert!(pattern_match("git push*", "git push origin main"));
        assert!(!pattern_match("orkia/**", "orkia-private/src/lib.rs"));
    }

    #[test]
    fn risk_order_is_monotonic() {
        assert!(risk_rank("critical") > risk_rank("high"));
        assert!(risk_rank("high") > risk_rank("medium"));
    }
}

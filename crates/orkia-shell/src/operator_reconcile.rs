// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Durable cross-session reconciliation for the operator (drift detector).
//!
//! The live operator (`crate::operator`) only ever sees one session's events
//! — a daemon-owned `@faye` and `@sage` each run in their own detached runtime
//! with their own per-process operator (the LPH boundary keeps tool events
//! local; only the final response forwards up). So no single live operator can
//! ever observe two agents touching the same artifact.
//!
//! The cross-session vantage point is therefore the **durable SEAL log**: every
//! agent seals its `hook.PreToolUse` actions into its per-job chain
//! (`agents/<agent>/jobs/<id>/seal.jsonl`), and the daemon can read them all.
//! This module rebuilds the multi-session graph from those chains and runs the
//! SAME `cross_session_hits` jointure the live operator uses — one detection
//! code path, fed either a live in-memory graph or the durable reconstruction.
//!
//! Notify-only and fail-closed: a missing or unreadable chain contributes
//! nothing; it never blocks an agent and never panics on malformed records.

use std::collections::HashMap;
use std::path::Path;

use orkia_rfc_core::RfcId;
use orkia_shell_types::JobId;

use crate::journal::{EventType, JournalEnvelope};
use crate::operator::{SessionState, is_contract_path, is_write_tool, pattern_match};

/// One cross-session intersection: a write by the actor session collides with
/// another session's scope. Shared between the live actor and the reconciler so
/// the verdict wording and `observed_action` shape stay identical on both paths.
pub(crate) struct CrossSessionHit {
    reason_tag: &'static str,
    affected_job: JobId,
    affected_agent: String,
    affected_rfc: Option<RfcId>,
}

impl CrossSessionHit {
    /// Human reason string — identical text on the live and durable paths.
    pub(crate) fn reason(&self, target: &str) -> String {
        format!(
            "write target '{target}' intersects {} for job {} ({})",
            self.reason_tag, self.affected_job.0, self.affected_agent
        )
    }

    /// Structured `observed_action` payload carried on the verdict.
    pub(crate) fn observed_action(&self, tool: &str, target: &str) -> serde_json::Value {
        serde_json::json!({
            "tool": tool,
            "target": target,
            "affected_job_id": self.affected_job.0,
            "affected_agent": self.affected_agent,
            "affected_rfc_id": self.affected_rfc.as_ref().map(RfcId::as_str),
        })
    }
}

/// THE cross-session jointure: does this write target intersect any OTHER
/// session's scope? Pure over the session graph — no IO, no `self` — so the
/// live operator and the durable reconciler call it unchanged.
pub(crate) fn cross_session_hits(
    sessions: &HashMap<JobId, SessionState>,
    actor_job: JobId,
    tool: &str,
    target: &str,
) -> Vec<CrossSessionHit> {
    if target.is_empty() || !is_write_tool(tool) {
        return Vec::new();
    }
    let mut hits: Vec<CrossSessionHit> = sessions
        .iter()
        .filter(|(job, _)| **job != actor_job)
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
            let reason_tag = if watched {
                "watch_paths"
            } else if exact_prior_touch {
                "same artifact touched by another session"
            } else if contract_area {
                "contract-like path overlaps another RFC scope"
            } else {
                return None;
            };
            Some(CrossSessionHit {
                reason_tag,
                affected_job: *job,
                affected_agent: state.agent.clone(),
                affected_rfc: state.rfc_id.clone(),
            })
        })
        .collect();
    // Stable order — HashMap iteration is nondeterministic and the reconciler
    // surfaces these rows to the user.
    hits.sort_by_key(|h| h.affected_job.0);
    hits
}

/// A session reconstructed from its durable SEAL chain: the graph view the
/// jointure reads (`state`), plus the write actions this session performed
/// (`writes`) to replay as the actor. `real_job` is the agent's own job id,
/// kept separate from the synthetic map key because per-agent job numbering
/// collides across agents (see `build_recon_sessions`).
struct ReconSession {
    state: SessionState,
    writes: Vec<(String, String)>,
    real_job: JobId,
}

/// Rebuild the multi-session graph from every per-job SEAL chain and emit a
/// `operator.cross_session_conflict` journal envelope for each intersection.
/// Read-only over durable evidence; safe to call on every `operator
/// events`/`status` invocation. Fail-soft on missing/broken chains.
pub(crate) fn reconcile_cross_session(data_dir: &Path) -> Vec<JournalEnvelope> {
    let sessions = build_recon_sessions(data_dir);
    let states: HashMap<JobId, SessionState> = sessions
        .iter()
        .map(|(job, recon)| (*job, recon.state.clone()))
        .collect();

    let mut out = Vec::new();
    for (job, recon) in &sessions {
        for (tool, target) in &recon.writes {
            for mut hit in cross_session_hits(&states, *job, tool, target) {
                // The jointure discriminates sessions by the synthetic map key;
                // translate the affected session back to its real per-agent job
                // id so the envelope and reason text carry the id the user sees.
                if let Some(affected) = sessions.get(&hit.affected_job) {
                    hit.affected_job = affected.real_job;
                }
                out.push(cross_session_envelope(
                    recon.real_job,
                    &recon.state,
                    tool,
                    target,
                    &hit,
                ));
            }
        }
    }
    out
}

fn build_recon_sessions(data_dir: &Path) -> HashMap<JobId, ReconSession> {
    let mut map = HashMap::new();
    // Synthetic, run-unique session key. The real per-agent job id CANNOT key
    // this map: each daemon-owned agent numbers its own chains from 1, so a
    // `faye` job 1 and a `sage` job 1 collide and the second collapses the
    // first — erasing the very second session a cross-session conflict needs.
    // Key by a counter; carry the real job id on the session for display.
    let mut next_key: u32 = 0;
    for summary in crate::seal::audit::list_job_chains(data_dir) {
        let path = data_dir
            .join("agents")
            .join(&summary.agent)
            .join("jobs")
            .join(summary.job_id.to_string())
            .join("seal.jsonl");
        let Ok(chain) = crate::seal::SealChain::load(path) else {
            continue;
        };
        let mut state = SessionState::for_agent(summary.agent.clone());
        let mut writes = Vec::new();
        for record in chain.records() {
            if record.event_type != "hook.PreToolUse" {
                continue;
            }
            let tool = record
                .detail
                .get("tool")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let target = record
                .detail
                .get("target")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if target.is_empty() {
                continue;
            }
            state.touched.push(target.to_string());
            if state.rfc_id.is_none() {
                state.rfc_id = record.rfc_id.clone();
            }
            if is_write_tool(tool) {
                writes.push((tool.to_string(), target.to_string()));
            }
        }
        if let Some(rfc) = &state.rfc_id {
            state.constraints = crate::operator_context::load_constraints(data_dir, rfc);
        }
        next_key += 1;
        map.insert(
            JobId(next_key),
            ReconSession {
                state,
                writes,
                real_job: JobId(summary.job_id),
            },
        );
    }
    map
}

/// Project a hit into the same envelope shape the live operator's journal sink
/// produces, so the operator builtins render reconciled conflicts identically.
fn cross_session_envelope(
    job: JobId,
    state: &SessionState,
    tool: &str,
    target: &str,
    hit: &CrossSessionHit,
) -> JournalEnvelope {
    let mut env = JournalEnvelope::now(EventType::Hook);
    env.source = Some("orkia-operator".into());
    env.event = Some("operator.cross_session_conflict".into());
    env.job_id = Some(job.0);
    if !state.agent.is_empty() {
        env.agent = Some(state.agent.clone());
    }
    env.message = Some(hit.reason(target));
    env.extra.insert(
        "kind".into(),
        serde_json::Value::String("cross_session_conflict".into()),
    );
    env.extra.insert(
        "severity".into(),
        serde_json::Value::String("warning".into()),
    );
    env.extra
        .insert("confidence".into(), serde_json::json!(1.0));
    env.extra.insert(
        "recommended_action".into(),
        serde_json::Value::String("notify_affected_session".into()),
    );
    env.extra
        .insert("observed_action".into(), hit.observed_action(tool, target));
    env.extra
        .insert("source_refs".into(), serde_json::Value::Array(Vec::new()));
    if let Some(rfc) = &state.rfc_id {
        env.extra.insert(
            "rfc_id".into(),
            serde_json::Value::String(rfc.as_str().to_string()),
        );
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_rfc_core::RfcStore;
    use orkia_rfc_core::frontmatter::{OperatorConstraints, OperatorFrontmatterBlock};
    use orkia_shell_types::JobId;
    use tempfile::tempdir;

    fn session(
        agent: &str,
        rfc: Option<&str>,
        constraints: Option<OperatorConstraints>,
        touched: &[&str],
    ) -> SessionState {
        let mut s = SessionState::for_agent(agent.into());
        s.rfc_id = rfc.map(RfcId::new);
        s.constraints = constraints;
        s.touched = touched.iter().map(|t| t.to_string()).collect();
        s
    }

    fn watch(paths: &[&str]) -> OperatorConstraints {
        OperatorConstraints {
            watch_paths: paths.iter().map(|p| p.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn hit_on_watched_path_of_another_session() {
        let mut sessions = HashMap::new();
        sessions.insert(
            JobId(2),
            session(
                "sage",
                Some("observer"),
                Some(watch(&["src/contracts/**"])),
                &[],
            ),
        );
        let hits = cross_session_hits(&sessions, JobId(1), "write_file", "src/contracts/auth.rs");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].affected_agent, "sage");
        assert!(
            hits[0]
                .reason("src/contracts/auth.rs")
                .contains("watch_paths")
        );
    }

    #[test]
    fn hit_on_same_artifact_touched_by_another_session() {
        let constraints = OperatorConstraints {
            allowed_paths: vec!["src/**".into()],
            ..Default::default()
        };
        let mut sessions = HashMap::new();
        sessions.insert(
            JobId(2),
            session("sage", Some("observer"), Some(constraints), &["src/api.rs"]),
        );
        let hits = cross_session_hits(&sessions, JobId(1), "write_file", "src/api.rs");
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0]
                .reason("src/api.rs")
                .contains("same artifact touched by another session")
        );
    }

    #[test]
    fn no_hit_without_constraints_or_for_self_or_for_reads() {
        let mut sessions = HashMap::new();
        // Other session has no constraints → cannot ground a conflict.
        sessions.insert(JobId(2), session("sage", None, None, &["src/api.rs"]));
        assert!(cross_session_hits(&sessions, JobId(1), "write_file", "src/api.rs").is_empty());
        // Same job → never conflicts with itself.
        sessions.insert(
            JobId(1),
            session("faye", Some("writer"), Some(watch(&["src/**"])), &[]),
        );
        assert!(cross_session_hits(&sessions, JobId(1), "write_file", "src/api.rs").is_empty());
        // Read tools never trigger cross-session.
        assert!(cross_session_hits(&sessions, JobId(3), "read_file", "src/api.rs").is_empty());
    }

    #[test]
    fn reconcile_is_fail_soft_on_empty_data_dir() {
        let dir = tempdir().expect("tmp");
        assert!(reconcile_cross_session(dir.path()).is_empty());
    }

    fn write_rfc(data_dir: &Path, slug: &str, constraints: OperatorConstraints) {
        let project = data_dir.join("projects").join("demo");
        std::fs::create_dir_all(&project).expect("project dir");
        let store = RfcStore::new(project);
        let id = RfcId::new(slug);
        let mut rec = store.create(&id, Some(slug)).expect("create rfc");
        rec.fm.operator = Some(OperatorFrontmatterBlock {
            constraints: Some(constraints),
        });
        store.save(rec.fm, format!("{slug} body")).expect("save");
    }

    fn seal_pretool(
        manager: &mut crate::seal::SealManager,
        job: u32,
        agent: &str,
        rfc: &str,
        tool: &str,
        target: &str,
    ) {
        manager.create_job_chain(JobId(job), agent);
        manager
            .seal_job_with_rfc(
                JobId(job),
                "hook.PreToolUse",
                serde_json::json!({"tool": tool, "target": target}),
                Some(RfcId::new(rfc)),
            )
            .expect("seal pretool");
    }

    #[test]
    fn reconcile_emits_cross_session_from_durable_chains() {
        let dir = tempdir().expect("tmp");
        // Observer watches the contract dir; writer writes into it.
        write_rfc(
            dir.path(),
            "writer",
            OperatorConstraints {
                allowed_paths: vec!["src/**".into()],
                ..Default::default()
            },
        );
        write_rfc(dir.path(), "observer", watch(&["src/contracts/**"]));

        let mut manager = crate::seal::SealManager::new(dir.path().to_path_buf());
        // Observer reads inside its own scope (establishes its session + rfc).
        seal_pretool(
            &mut manager,
            2,
            "sage",
            "observer",
            "read_file",
            "tests/x.rs",
        );
        // Writer writes into the watched contract path.
        seal_pretool(
            &mut manager,
            1,
            "faye",
            "writer",
            "write_file",
            "src/contracts/auth.rs",
        );

        let envelopes = reconcile_cross_session(dir.path());
        assert_eq!(envelopes.len(), 1, "expected one cross-session conflict");
        let env = &envelopes[0];
        assert_eq!(
            env.event.as_deref(),
            Some("operator.cross_session_conflict")
        );
        assert_eq!(env.job_id, Some(1));
        assert_eq!(env.agent.as_deref(), Some("faye"));
        assert!(
            env.message
                .as_deref()
                .unwrap_or_default()
                .contains("watch_paths"),
            "{:?}",
            env.message
        );
        assert_eq!(
            env.extra
                .get("observed_action")
                .and_then(|o| o.get("affected_agent"))
                .and_then(|v| v.as_str()),
            Some("sage")
        );
    }

    #[test]
    fn reconcile_distinguishes_two_agents_that_share_job_id_one() {
        // Real daemon-owned agents each number their OWN SEAL chains from 1, so
        // two live agents both carry job id 1 on disk. Keying the session graph
        // by job id alone collapses them into one session and silently drops
        // every cross-session conflict — the structural bug the demo exposed.
        let dir = tempdir().expect("tmp");
        write_rfc(dir.path(), "guard", watch(&["shared/**", "shared/*"]));

        // Separate managers == separate processes: each writes under its own
        // agent dir, BOTH at jobs/1.
        let mut faye = crate::seal::SealManager::new(dir.path().to_path_buf());
        seal_pretool(
            &mut faye,
            1,
            "faye",
            "guard",
            "write_file",
            "guard/init.txt",
        );
        let mut sage = crate::seal::SealManager::new(dir.path().to_path_buf());
        seal_pretool(
            &mut sage,
            1,
            "sage",
            "guard",
            "write_file",
            "shared/notes.txt",
        );

        let envelopes = reconcile_cross_session(dir.path());
        assert_eq!(
            envelopes.len(),
            1,
            "two job-id-1 agents must not collapse: {envelopes:?}"
        );
        let env = &envelopes[0];
        assert_eq!(
            env.event.as_deref(),
            Some("operator.cross_session_conflict")
        );
        // Actor = the agent that wrote INTO the watched area.
        assert_eq!(env.agent.as_deref(), Some("sage"));
        assert!(
            env.message
                .as_deref()
                .unwrap_or_default()
                .contains("watch_paths"),
            "{:?}",
            env.message
        );
        assert_eq!(
            env.extra
                .get("observed_action")
                .and_then(|o| o.get("affected_agent"))
                .and_then(|v| v.as_str()),
            Some("faye")
        );
    }
}

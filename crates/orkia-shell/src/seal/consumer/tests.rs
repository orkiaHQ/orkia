// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::protocol::{EventPayload, EventSource};
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn mk_manager(dir: &std::path::Path) -> SealManager {
        SealManager::new(dir.to_path_buf())
    }

    fn mk_projects() -> JobProjects {
        Arc::new(RwLock::new(HashMap::new()))
    }

    /// Internal terse alias used by the existing tests in this module.
    /// New tests that care about the scheduled context call `route`
    /// directly with their own `ScheduledContext`.
    fn route_default(manager: &mut SealManager, job_projects: &JobProjects, event: OrkiaEvent) {
        route(manager, job_projects, &ScheduledContext::default(), event).expect("route");
    }

    fn evt(source: EventSource, job_id: JobId, agent: &str, payload: EventPayload) -> OrkiaEvent {
        OrkiaEvent {
            source,
            event: payload,
            confidence: 1.0,
            timestamp: chrono::Utc::now(),
            job_id,
            agent_name: agent.to_string(),
            rfc_id: None,
        }
    }

    #[test]
    fn agent_spawn_creates_job_chain_with_genesis() {
        let dir = tempdir().unwrap();
        let mut mgr = mk_manager(dir.path());
        let projects = mk_projects();
        route_default(
            &mut mgr,
            &projects,
            evt(
                EventSource::Internal,
                JobId(1),
                "faye",
                EventPayload::Custom {
                    name: "agent.spawn".into(),
                    data: serde_json::json!({"tools_count": 3, "system_prompt_hash": "abc"}),
                },
            ),
        );
        let chain = mgr.job_chain(JobId(1)).expect("chain created");
        assert_eq!(chain.len(), 1);
        assert_eq!(chain.records()[0].event_type, "agent.spawn");
    }

    #[test]
    fn session_end_closes_and_links_to_project() {
        let dir = tempdir().unwrap();
        let mut mgr = mk_manager(dir.path());
        let projects = mk_projects();
        projects.write().insert(JobId(2), "orkia-shell".to_string());

        for payload in [
            EventPayload::Custom {
                name: "agent.spawn".into(),
                data: serde_json::json!({}),
            },
            EventPayload::ToolUse {
                tool: "Read".into(),
                target: Some("foo".into()),
                input_summary: None,
            },
            EventPayload::SessionEnd { exit_code: Some(0) },
        ] {
            route_default(
                &mut mgr,
                &projects,
                evt(EventSource::Internal, JobId(2), "faye", payload),
            );
        }

        // Job chain evicted after close.
        assert!(mgr.job_chain(JobId(2)).is_none());
        // Project chain got the job.reference.
        let project = mgr.project_chain("orkia-shell").expect("project chain");
        assert_eq!(project.len(), 1);
        assert_eq!(project.records()[0].event_type, "job.reference");
        // Cleaned up the job_projects entry.
        assert!(!projects.read().contains_key(&JobId(2)));
    }

    #[test]
    fn rfc_event_lands_on_project_chain_via_payload() {
        let dir = tempdir().unwrap();
        let mut mgr = mk_manager(dir.path());
        let projects = mk_projects();
        // No job in projects map — relying on `data.project` to
        // route to the right chain. Mirrors how RFC builtins
        // emit (no active job).
        route_default(
            &mut mgr,
            &projects,
            evt(
                EventSource::Internal,
                JobId(0),
                "",
                EventPayload::Custom {
                    name: "rfc.create".into(),
                    data: serde_json::json!({
                        "slug": "implement-x",
                        "project": "orkia-shell",
                        "content_hash": "abc1234567890def",
                    }),
                },
            ),
        );
        let project = mgr.project_chain("orkia-shell").expect("created");
        assert_eq!(project.len(), 1);
        assert_eq!(project.records()[0].event_type, "rfc.create");
    }

    #[test]
    fn hook_events_route_to_job_chain() {
        let dir = tempdir().unwrap();
        let mut mgr = mk_manager(dir.path());
        let projects = mk_projects();
        // Spawn first so the chain exists.
        route_default(
            &mut mgr,
            &projects,
            evt(
                EventSource::Internal,
                JobId(3),
                "faye",
                EventPayload::Custom {
                    name: "agent.spawn".into(),
                    data: serde_json::json!({}),
                },
            ),
        );
        route_default(
            &mut mgr,
            &projects,
            evt(
                EventSource::Hook {
                    provider: "claude".into(),
                },
                JobId(3),
                "faye",
                EventPayload::ToolUse {
                    tool: "Bash".into(),
                    target: Some("cargo test".into()),
                    input_summary: None,
                },
            ),
        );
        route_default(
            &mut mgr,
            &projects,
            evt(
                EventSource::Hook {
                    provider: "claude".into(),
                },
                JobId(3),
                "faye",
                EventPayload::ToolResult {
                    tool: "Bash".into(),
                    target: Some("cargo test".into()),
                    exit_code: Some(0),
                    output_summary: None,
                },
            ),
        );
        let chain = mgr.job_chain(JobId(3)).expect("chain");
        let types: Vec<_> = chain
            .records()
            .iter()
            .map(|r| r.event_type.clone())
            .collect();
        assert_eq!(
            types,
            vec!["agent.spawn", "hook.PreToolUse", "hook.PostToolUse"]
        );
    }

    #[test]
    fn user_message_from_state_machine_records_prompt_injected() {
        let dir = tempdir().unwrap();
        let mut mgr = mk_manager(dir.path());
        let projects = mk_projects();
        route_default(
            &mut mgr,
            &projects,
            evt(
                EventSource::Internal,
                JobId(4),
                "faye",
                EventPayload::Custom {
                    name: "agent.spawn".into(),
                    data: serde_json::json!({}),
                },
            ),
        );
        route_default(
            &mut mgr,
            &projects,
            evt(
                EventSource::StateMachine,
                JobId(4),
                "faye",
                EventPayload::UserMessage {
                    text: "fix the tests".into(),
                },
            ),
        );
        let chain = mgr.job_chain(JobId(4)).expect("chain");
        assert_eq!(chain.records()[1].event_type, "prompt_injected");
        assert_eq!(
            chain.records()[1]
                .detail
                .get("len")
                .and_then(|v| v.as_u64()),
            Some(13)
        );
    }

    // Silence the unused-import warning when tests pass without
    // exercising PathBuf directly; keeps the import group tidy.
    #[allow(dead_code)]
    fn _unused_pathbuf_keepalive() -> PathBuf {
        PathBuf::new()
    }

    /// Approval-required success under a scheduled context: must
    /// write `~/.orkia/pending/<job_id>.json` + append an
    /// `approval_pending` line to `journal.jsonl`.
    #[test]
    fn scheduled_success_with_approval_parks_and_notifies() {
        let dir = tempdir().unwrap();
        let mut mgr = mk_manager(dir.path());
        let projects = mk_projects();
        let sched = ScheduledContext {
            is_scheduled: true,
            approval_required: true,
        };

        // Spawn + close cycle.
        route(
            &mut mgr,
            &projects,
            &sched,
            evt(
                EventSource::Internal,
                JobId(101),
                "strict",
                EventPayload::Custom {
                    name: "agent.spawn".into(),
                    data: serde_json::json!({}),
                },
            ),
        )
        .expect("route");
        route(
            &mut mgr,
            &projects,
            &sched,
            evt(
                EventSource::Internal,
                JobId(101),
                "strict",
                EventPayload::SessionEnd { exit_code: Some(0) },
            ),
        )
        .expect("route");

        let pending = dir.path().join("pending").join("101.json");
        assert!(pending.exists(), "pending JSON must be written");
        let journal = dir.path().join("journal.jsonl");
        let body = std::fs::read_to_string(&journal).expect("journal exists");
        assert!(
            body.contains("\"approval_pending\""),
            "journal must record approval_pending"
        );
        assert!(body.contains("\"origin\":\"scheduled\""));
    }

    /// Non-zero exit under a scheduled context: must append a
    /// `scheduled_failure` lifecycle event to `journal.jsonl` and
    /// NOT park a pending result.
    #[test]
    fn scheduled_failure_emits_journal_event_only() {
        let dir = tempdir().unwrap();
        let mut mgr = mk_manager(dir.path());
        let projects = mk_projects();
        let sched = ScheduledContext {
            is_scheduled: true,
            approval_required: true,
        };

        route(
            &mut mgr,
            &projects,
            &sched,
            evt(
                EventSource::Internal,
                JobId(202),
                "strict",
                EventPayload::Custom {
                    name: "agent.spawn".into(),
                    data: serde_json::json!({}),
                },
            ),
        )
        .expect("route");
        route(
            &mut mgr,
            &projects,
            &sched,
            evt(
                EventSource::Internal,
                JobId(202),
                "strict",
                EventPayload::SessionEnd { exit_code: Some(1) },
            ),
        )
        .expect("route");

        let body = std::fs::read_to_string(dir.path().join("journal.jsonl")).unwrap();
        assert!(body.contains("\"scheduled_failure\""));
        assert!(
            !dir.path().join("pending").join("202.json").exists(),
            "failures must not park a pending result"
        );
    }

    /// Non-scheduled run (default ctx) takes neither branch — close
    #[test]
    fn non_scheduled_close_writes_no_pending_or_journal() {
        let dir = tempdir().unwrap();
        let mut mgr = mk_manager(dir.path());
        let projects = mk_projects();
        route_default(
            &mut mgr,
            &projects,
            evt(
                EventSource::Internal,
                JobId(303),
                "faye",
                EventPayload::Custom {
                    name: "agent.spawn".into(),
                    data: serde_json::json!({}),
                },
            ),
        );
        route_default(
            &mut mgr,
            &projects,
            evt(
                EventSource::Internal,
                JobId(303),
                "faye",
                EventPayload::SessionEnd { exit_code: Some(0) },
            ),
        );
        assert!(!dir.path().join("pending").exists());
        assert!(!dir.path().join("journal.jsonl").exists());
    }
}

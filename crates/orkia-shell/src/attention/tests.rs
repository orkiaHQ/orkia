use super::*;

#[test]
fn hint_prefers_blocking() {
    let row = AttentionRow {
        id: AttentionId(1),
        job_id: Some(1),
        agent: "faye".into(),
        kind: AttentionKind::BlockingApproval,
        severity: AttentionSeverity::Blocking,
        age: "now".into(),
        summary: "approval".into(),
        actions: vec![],
    };
    assert_eq!(hint_for(&[row]), Some(AttentionHint::Blocking { count: 1 }));
}

#[test]
fn severity_tiers_match_age_thresholds() {
    assert_eq!(
        severity_for_age(chrono::Duration::minutes(14)),
        AttentionSeverity::Fresh
    );
    assert_eq!(
        severity_for_age(chrono::Duration::minutes(15)),
        AttentionSeverity::Muted
    );
    assert_eq!(
        severity_for_age(chrono::Duration::minutes(30)),
        AttentionSeverity::Warning
    );
    assert_eq!(
        severity_for_age(chrono::Duration::minutes(60)),
        AttentionSeverity::Overdue
    );
}

#[test]
fn detects_file_conflict_from_hooks() {
    let mut state = State::default();
    let read = hook(1, "sage", "Read", "src/auth.rs");
    state.observe_hook(&read);

    let write = hook(2, "faye", "Write", "src/auth.rs");
    state.observe_hook(&write);

    let rows = state.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, AttentionKind::ResourceConflict);
}

#[test]
fn multiple_agents_hint_uses_oldest_count() {
    let mut state = State::default();
    state.apply(Command::AgentPrompt(AgentPromptInput {
        job_id: JobId(1),
        agent: "faye".into(),
        summary: "question".into(),
        pending_body: None,
    }));
    state.apply(Command::AgentPrompt(AgentPromptInput {
        job_id: JobId(2),
        agent: "sage".into(),
        summary: "question".into(),
        pending_body: None,
    }));

    let rows = state.rows();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        hint_for(&rows),
        Some(AttentionHint::Passive(
            "(2 queued · oldest now · ^G)".into()
        ))
    );
}

#[test]
fn blocking_approval_resolves_with_approval_effect() {
    let mut state = State::default();
    state.apply(Command::BlockingApproval(BlockingApprovalInput {
        job_id: JobId(7),
        agent: "faye".into(),
        action: "Write src/auth.rs".into(),
        risk: "write".into(),
    }));
    let rows = state.rows();
    assert_eq!(hint_for(&rows), Some(AttentionHint::Blocking { count: 1 }));

    let result = state.resolve(rows[0].id, "allow");
    assert_eq!(
        result.effect,
        AttentionResolveEffect::Approval {
            job_id: 7,
            approved: true,
        }
    );
    assert!(state.rows().is_empty());
}

#[test]
fn conflict_detects_parent_child_paths() {
    let mut state = State::default();
    state.observe_hook(&hook(1, "sage", "Read", "src"));
    state.observe_hook(&hook(2, "faye", "Write", "src/auth.rs"));

    let rows = state.rows();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].summary.contains("src/auth.rs"));
}

#[test]
fn conflict_ignores_non_overlapping_paths() {
    let mut state = State::default();
    state.observe_hook(&hook(1, "sage", "Read", "src/auth.rs"));
    state.observe_hook(&hook(2, "faye", "Write", "docs/auth.md"));

    assert!(state.rows().is_empty());
}

#[test]
fn malformed_hook_payload_is_ignored() {
    let mut state = State::default();
    let mut env = hook(1, "sage", "Read", "");
    env.target = None;
    env.extra
        .insert("paths".into(), serde_json::json!([null, 42, {"bad": true}]));
    state.observe_hook(&env);

    assert!(state.rows().is_empty());
}

#[test]
fn bash_command_paths_create_conflict() {
    let mut state = State::default();
    let mut test = hook(1, "sage", "Bash", "");
    test.target = None;
    test.description = Some("cargo test src/auth.rs".into());
    state.observe_hook(&test);

    let mut add = hook(2, "faye", "Bash", "");
    add.target = None;
    add.extra
        .insert("command".into(), serde_json::json!("git add src/auth.rs"));
    state.observe_hook(&add);

    let rows = state.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, AttentionKind::ResourceConflict);
}

#[test]
fn post_tool_use_closes_long_read() {
    let mut state = State::default();
    state.observe_hook(&hook(1, "sage", "Read", "src/auth.rs"));
    let mut done = hook(1, "sage", "Read", "src/auth.rs");
    done.event = Some("PostToolUse".into());
    state.observe_hook(&done);
    state.observe_hook(&hook(2, "faye", "Write", "src/auth.rs"));

    assert!(state.rows().is_empty());
}

#[test]
fn hold_keeps_conflict_active_and_proceed_releases_it() {
    let mut state = State::default();
    state.observe_hook(&hook(1, "sage", "Read", "src/auth.rs"));
    state.observe_hook(&hook(2, "faye", "Write", "src/auth.rs"));
    let id = state.rows()[0].id;

    let held = state.resolve(id, "hold");
    assert_eq!(held.effect, AttentionResolveEffect::HoldJob(2));
    assert_eq!(state.rows().len(), 1);

    let released = state.resolve(id, "proceed-anyway");
    assert_eq!(released.effect, AttentionResolveEffect::ReleaseJob(2));
    assert!(state.rows().is_empty());
}

fn hook(job_id: u32, agent: &str, tool: &str, target: &str) -> JournalEnvelope {
    let mut env = JournalEnvelope::now(orkia_shell_types::EventType::Hook);
    env.event = Some("PreToolUse".into());
    env.tool = Some(tool.into());
    env.target = Some(target.into());
    env.job_id = Some(job_id);
    env.agent = Some(agent.into());
    env
}

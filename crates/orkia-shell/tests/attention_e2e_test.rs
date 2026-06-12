// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use std::thread;
use std::time::{Duration, Instant};

use orkia_shell::attention::{AgentPromptInput, AttentionCoordinator, BlockingApprovalInput};
use orkia_shell_types::attention::{
    AttentionControl, AttentionHint, AttentionKind, AttentionResolveEffect,
};
use orkia_shell_types::{EventType, JobId, JournalEnvelope};

#[test]
fn prompt_attention_flows_through_public_coordinator() {
    let attention = AttentionCoordinator::spawn();
    attention.agent_prompt(AgentPromptInput {
        job_id: JobId(1),
        agent: "faye".into(),
        summary: "agent asks for input".into(),
        pending_body: None,
    });

    let rows = wait_rows(&attention, 1);
    assert_eq!(rows[0].agent, "faye");
    assert_eq!(rows[0].kind, AttentionKind::AgentPrompt);
    assert_eq!(
        attention.hint(),
        Some(AttentionHint::Passive("(@faye queued · ^G)".into()))
    );

    let pulled = attention.pull();
    assert_eq!(pulled.rows.len(), 1);
    assert_eq!(pulled.rows[0].kind, AttentionKind::AgentPrompt);
}

#[test]
fn multiple_agent_prompts_surface_single_counted_hint() {
    let attention = AttentionCoordinator::spawn();
    attention.agent_prompt(AgentPromptInput {
        job_id: JobId(1),
        agent: "faye".into(),
        summary: "first".into(),
        pending_body: None,
    });
    attention.agent_prompt(AgentPromptInput {
        job_id: JobId(2),
        agent: "sage".into(),
        summary: "second".into(),
        pending_body: None,
    });

    let rows = wait_rows(&attention, 2);
    assert_eq!(rows.len(), 2);
    assert_eq!(
        attention.hint(),
        Some(AttentionHint::Passive(
            "(2 queued · oldest now · ^G)".into()
        ))
    );
}

#[test]
fn blocking_approval_hint_clears_after_resolution() {
    let attention = AttentionCoordinator::spawn();
    attention.blocking_approval(BlockingApprovalInput {
        job_id: JobId(9),
        agent: "faye".into(),
        action: "Write".into(),
        risk: "high".into(),
    });

    let rows = wait_rows(&attention, 1);
    assert_eq!(attention.hint(), Some(AttentionHint::Blocking { count: 1 }));

    let resolved = attention.resolve(rows[0].id, "deny");
    assert_eq!(
        resolved.effect,
        AttentionResolveEffect::Approval {
            job_id: 9,
            approved: false
        }
    );
    wait_until(Duration::from_secs(1), || attention.rows().is_empty());
    assert_eq!(attention.hint(), None);
}

#[test]
fn hook_conflict_supports_hold_and_proceed() {
    let attention = AttentionCoordinator::spawn();
    attention.observe_hook(&hook(1, "sage", "Read", "src/auth.rs"));
    attention.observe_hook(&hook(2, "faye", "Write", "src/auth.rs"));

    let rows = wait_rows(&attention, 1);
    assert_eq!(rows[0].kind, AttentionKind::ResourceConflict);
    assert_eq!(
        attention.hint(),
        Some(AttentionHint::Passive("(1 queued · conflict · ^G)".into()))
    );

    let held = attention.resolve(rows[0].id, "hold");
    assert_eq!(held.effect, AttentionResolveEffect::HoldJob(2));
    assert_eq!(wait_rows(&attention, 1).len(), 1);

    let released = attention.resolve(rows[0].id, "proceed-anyway");
    assert_eq!(released.effect, AttentionResolveEffect::ReleaseJob(2));
    wait_until(Duration::from_secs(1), || attention.rows().is_empty());
}

fn wait_rows(
    attention: &AttentionCoordinator,
    count: usize,
) -> Vec<orkia_shell_types::attention::AttentionRow> {
    let mut rows = Vec::new();
    wait_until(Duration::from_secs(1), || {
        rows = attention.rows();
        rows.len() >= count
    });
    rows
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(predicate(), "condition was not met within {timeout:?}");
}

fn hook(job_id: u32, agent: &str, tool: &str, target: &str) -> JournalEnvelope {
    let mut env = JournalEnvelope::now(EventType::Hook);
    env.event = Some("PreToolUse".into());
    env.tool = Some(tool.into());
    env.target = Some(target.into());
    env.job_id = Some(job_id);
    env.agent = Some(agent.into());
    env
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Pending prompt queue: tracks the user's intent (`@faye fix tests`)
//! across the agent's startup boot prompts so the body is injected
//! once the agent is actually ready to receive it.
//!
//! State transitions (driven by the detector + REPL):
//!
//!   WaitingForBoot
//!       │ a non-shell prompt is detected
//!       ▼
//!   WaitingForApproval
//!       │ user runs `approve N` (or V2 auto-resolve fires)
//!       ▼
//!   WaitingForReady
//!       │ a ShellPrompt is detected (or 10s pass with high confidence)
//!       ▼
//!   ReadyToInject
//!       │ REPL writes body to PTY
//!       ▼
//!   Injected
//!
//! `cleanup` is called when the job exits. If the entry was not yet
//! Injected, the caller receives a [`DroppedPrompt`] so it can emit
//! the `prompt_dropped` journal event.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use orkia_shell_types::JobId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingState {
    /// Just spawned, possibly with a body. Waiting for first boot
    /// prompt (trust check, etc.) OR for the agent to reach ready
    /// without a boot prompt at all.
    WaitingForBoot,
    /// A non-shell prompt is on screen; user / auto-resolver must
    /// answer.
    WaitingForApproval,
    /// Boot resolved (or no boot prompt). Agent is rendering — we
    /// hold the next body until it settles (idle ≥ 1.5 s).
    WaitingForReady,
    /// Detector decided agent is idle and there is a body to inject.
    /// `take_ready` pops the front body and either transitions back
    /// to `WaitingForReady` (more bodies queued — claude needs to
    /// render the previous one first) or to `Idle` (queue empty).
    ReadyToInject,
    /// Queue is empty, agent is alive and free. The next
    /// `append_body` flips us back to `WaitingForReady`.
    Idle,
}

#[derive(Debug, Clone)]
pub struct PendingPrompt {
    /// FIFO queue of bodies to deliver. Each `take_ready` pops the
    /// front. `append_body` pushes to the back. Empty queue = state
    /// must be `Idle` (or one of the boot states with no body).
    pub bodies: VecDeque<String>,
    pub agent_name: String,
    pub created_at: Instant,
    pub state: PendingState,
    /// When [`Self::state`] last transitioned to `WaitingForReady`.
    /// Reset on every re-entry to `WaitingForReady` (e.g. after a
    /// `take_ready` that leaves more bodies queued).
    pub ready_since: Option<Instant>,
    pub boot_prompts_resolved: u32,
}

/// Returned by [`PendingPromptQueue::cleanup`] when a job exits with
/// bodies still queued. The REPL emits a `prompt_dropped` journal
/// entry per body + a user-visible toast.
#[derive(Debug, Clone)]
pub struct DroppedPrompt {
    pub job_id: JobId,
    pub agent_name: String,
    pub bodies: Vec<String>,
    pub state_at_drop: PendingState,
}

pub struct PendingPromptQueue {
    entries: HashMap<JobId, PendingPrompt>,
}

impl PendingPromptQueue {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Register a fresh agent job with an optional first body. The
    /// entry persists even after the body is delivered, so follow-up
    /// `append_body` calls can queue more prompts on the same agent.
    pub fn enqueue(&mut self, job_id: JobId, body: Option<String>, agent_name: String) {
        let mut bodies = VecDeque::new();
        if let Some(b) = body {
            bodies.push_back(b);
        }
        self.entries.insert(
            job_id,
            PendingPrompt {
                bodies,
                agent_name,
                created_at: Instant::now(),
                state: PendingState::WaitingForBoot,
                ready_since: None,
                boot_prompts_resolved: 0,
            },
        );
    }

    /// Append a follow-up body to a running agent's queue. Returns
    /// `true` when the entry exists and the body was appended; the
    /// state is bumped to `WaitingForReady` if the queue was idle so
    /// the detector picks it up.
    pub fn append_body(&mut self, job_id: JobId, body: String) -> bool {
        let Some(e) = self.entries.get_mut(&job_id) else {
            return false;
        };
        e.bodies.push_back(body);
        if e.state == PendingState::Idle {
            e.state = PendingState::WaitingForReady;
            e.ready_since = Some(Instant::now());
        }
        true
    }

    /// Count of un-delivered bodies for the job. `0` when idle.
    pub fn pending_count(&self, job_id: JobId) -> usize {
        self.entries
            .get(&job_id)
            .map(|e| e.bodies.len())
            .unwrap_or(0)
    }

    /// A boot prompt (non-shell) was detected. Transition is only
    /// valid from `WaitingForBoot` / `WaitingForReady`. `Idle` is
    /// untouched (agent has no body queued — boot prompts after
    /// idle would be a state we don't model).
    pub fn on_boot_prompt_detected(&mut self, job_id: JobId) {
        if let Some(e) = self.entries.get_mut(&job_id)
            && matches!(
                e.state,
                PendingState::WaitingForBoot | PendingState::WaitingForReady
            )
        {
            e.state = PendingState::WaitingForApproval;
            e.ready_since = None;
        }
    }

    /// User / auto-resolve handled the boot prompt.
    pub fn on_boot_prompt_resolved(&mut self, job_id: JobId) {
        if let Some(e) = self.entries.get_mut(&job_id)
            && e.state == PendingState::WaitingForApproval
        {
            e.boot_prompts_resolved = e.boot_prompts_resolved.saturating_add(1);
            e.state = PendingState::WaitingForReady;
            e.ready_since = Some(Instant::now());
        }
    }

    /// A shell prompt was classified — agent is ready for input.
    pub fn on_agent_ready(&mut self, job_id: JobId) {
        if let Some(e) = self.entries.get_mut(&job_id)
            && matches!(
                e.state,
                PendingState::WaitingForBoot | PendingState::WaitingForReady
            )
        {
            // Only transition to ReadyToInject if there's actually a
            // body to inject. Otherwise the agent is just idle and
            // we should stay in WaitingForReady / move to Idle.
            if e.bodies.is_empty() {
                e.state = PendingState::Idle;
            } else {
                e.state = PendingState::ReadyToInject;
            }
        }
    }

    /// Force-inject when the classifier can't pin a ShellPrompt but
    /// other signals say the agent is ready. TUI agents (claude,
    /// codex) draw their "ready" UI with cursor positioning rather
    /// than LF-terminated lines, so the `❯`/`$` heuristic in
    /// `classifier::classify` misses the ready state.
    ///
    /// We fire when:
    ///   * state is `WaitingForReady` (boot already resolved)
    ///   * `idle_duration >= 1.5s` — the agent has stopped drawing
    ///   * `>=1s` has passed since `ready_since` — boot resolution
    ///     had time to take effect
    ///
    /// Driven by the detector tick with `signals.idle_duration()`,
    /// not by a wall-clock timer — adapts to how long claude actually
    /// takes to render its welcome.
    pub fn force_inject_if_idle(&mut self, job_id: JobId, idle: Duration) -> bool {
        let Some(e) = self.entries.get_mut(&job_id) else {
            return false;
        };
        if e.state != PendingState::WaitingForReady {
            return false;
        }
        if e.bodies.is_empty() {
            // No body to deliver — agent is just idle. Move to Idle.
            e.state = PendingState::Idle;
            return false;
        }
        let Some(since) = e.ready_since else {
            return false;
        };
        if since.elapsed() < Duration::from_secs(1) {
            return false;
        }
        if idle < Duration::from_millis(1500) {
            return false;
        }
        e.state = PendingState::ReadyToInject;
        true
    }

    /// Boot-time idle inject for alt-screen TUI agents. claude/codex
    /// launched with `--dangerously-skip-permissions` show no boot
    /// prompt, so nothing drives `WaitingForBoot → WaitingForReady`;
    /// they render their own ready UI as an alt-screen and go idle. That
    /// idle alt-screen IS the ready signal.
    ///
    /// Safe ONLY from `WaitingForBoot`: in that state the agent has never
    /// received a body, so it cannot have opened a nested editor/pager —
    /// the alt-screen can only be its own welcome UI. Once a body is
    /// delivered the state leaves `WaitingForBoot` for good and the
    /// detector's alt-screen early-return resumes protecting against
    /// injecting into a real nested editor.
    ///
    /// Fires when:
    ///   * state is `WaitingForBoot` with a body queued
    ///   * `>= 2s` since spawn — boot render had time to start
    ///   * `idle >= 1.5s` — the agent has stopped drawing
    pub fn boot_inject_if_idle(&mut self, job_id: JobId, idle: Duration) -> bool {
        let Some(e) = self.entries.get_mut(&job_id) else {
            return false;
        };
        if e.state != PendingState::WaitingForBoot || e.bodies.is_empty() {
            return false;
        }
        if e.created_at.elapsed() < Duration::from_secs(2) {
            return false;
        }
        if idle < Duration::from_millis(1500) {
            return false;
        }
        e.state = PendingState::ReadyToInject;
        true
    }

    /// Pop the front body and transition state. When the queue has
    /// more bodies left, we go back to `WaitingForReady` (claude
    /// needs to render the previous response then settle before the
    /// next force-inject); when empty, `Idle`.
    pub fn take_ready(&mut self, job_id: JobId) -> Option<String> {
        let e = self.entries.get_mut(&job_id)?;
        if e.state != PendingState::ReadyToInject {
            return None;
        }
        let body = e.bodies.pop_front()?;
        if e.bodies.is_empty() {
            e.state = PendingState::Idle;
            e.ready_since = None;
        } else {
            e.state = PendingState::WaitingForReady;
            e.ready_since = Some(Instant::now());
        }
        Some(body)
    }

    /// True when the job has any *un-delivered* body queued.
    pub fn has_pending(&self, job_id: JobId) -> bool {
        self.entries
            .get(&job_id)
            .map(|e| !e.bodies.is_empty())
            .unwrap_or(false)
    }

    /// Peek at the next body that will be delivered (the head of the
    /// queue). `None` when there is nothing to deliver.
    pub fn pending_body(&self, job_id: JobId) -> Option<&str> {
        self.entries
            .get(&job_id)
            .and_then(|e| e.bodies.front())
            .map(String::as_str)
    }

    pub fn agent_name(&self, job_id: JobId) -> Option<&str> {
        self.entries.get(&job_id).map(|e| e.agent_name.as_str())
    }

    pub fn state(&self, job_id: JobId) -> Option<&PendingState> {
        self.entries.get(&job_id).map(|e| &e.state)
    }

    /// Called by REPL on job exit. Returns `Some(DroppedPrompt)`
    /// when bodies remained in the queue that were never delivered.
    pub fn cleanup(&mut self, job_id: JobId) -> Option<DroppedPrompt> {
        let entry = self.entries.remove(&job_id)?;
        if entry.bodies.is_empty() {
            return None;
        }
        Some(DroppedPrompt {
            job_id,
            agent_name: entry.agent_name,
            bodies: entry.bodies.into_iter().collect(),
            state_at_drop: entry.state,
        })
    }

    /// Read-only snapshot for the `attention list` builtin: returns
    /// every queue entry as `(job_id, agent_name, peek_body, state)`.
    /// `peek_body` is the head of the queue (next to deliver) — empty
    /// string if the queue is idle.
    pub fn entries_internal(&self) -> Vec<(JobId, String, String, PendingState)> {
        self.entries
            .iter()
            .map(|(id, e)| {
                let peek = e.bodies.front().cloned().unwrap_or_default();
                (*id, e.agent_name.clone(), peek, e.state.clone())
            })
            .collect()
    }

    /// Entries that have bodies queued for longer than 60s. Used by
    /// the REPL to emit a stale warning. `Idle` entries (empty
    /// queue) are skipped — no body, no timeout.
    pub fn check_timeouts(&self) -> Vec<(JobId, Duration)> {
        let now = Instant::now();
        self.entries
            .iter()
            .filter(|(_, e)| !e.bodies.is_empty())
            .filter_map(|(&id, e)| {
                let age = now.duration_since(e.created_at);
                (age > Duration::from_secs(60)).then_some((id, age))
            })
            .collect()
    }
}

impl Default for PendingPromptQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: u32) -> JobId {
        JobId(id)
    }

    #[test]
    fn normal_lifecycle() {
        let mut q = PendingPromptQueue::new();
        q.enqueue(job(1), Some("fix tests".into()), "faye".into());
        assert_eq!(q.state(job(1)), Some(&PendingState::WaitingForBoot));

        q.on_boot_prompt_detected(job(1));
        assert_eq!(q.state(job(1)), Some(&PendingState::WaitingForApproval));

        q.on_boot_prompt_resolved(job(1));
        assert_eq!(q.state(job(1)), Some(&PendingState::WaitingForReady));

        q.on_agent_ready(job(1));
        assert_eq!(q.take_ready(job(1)).as_deref(), Some("fix tests"));
        // Queue empty → Idle (entry stays alive for follow-ups).
        assert_eq!(q.state(job(1)), Some(&PendingState::Idle));
    }

    #[test]
    fn no_boot_prompt_path() {
        // Agent goes straight to ready.
        let mut q = PendingPromptQueue::new();
        q.enqueue(job(1), Some("hello".into()), "faye".into());
        q.on_agent_ready(job(1));
        assert_eq!(q.take_ready(job(1)).as_deref(), Some("hello"));
    }

    #[test]
    fn append_body_queues_for_running_agent() {
        let mut q = PendingPromptQueue::new();
        q.enqueue(job(1), Some("first".into()), "faye".into());
        q.on_agent_ready(job(1));
        assert_eq!(q.take_ready(job(1)).as_deref(), Some("first"));
        // Now Idle. Append a follow-up.
        assert!(q.append_body(job(1), "second".into()));
        assert_eq!(q.state(job(1)), Some(&PendingState::WaitingForReady));
        assert_eq!(q.pending_count(job(1)), 1);
        // Force-inject after 1s aging + idle.
        let entry = q.entries.get_mut(&job(1)).expect("present");
        entry.ready_since = Some(Instant::now() - Duration::from_secs(2));
        assert!(q.force_inject_if_idle(job(1), Duration::from_millis(1600)));
        assert_eq!(q.take_ready(job(1)).as_deref(), Some("second"));
    }

    #[test]
    fn multi_body_queue_drains_in_order() {
        let mut q = PendingPromptQueue::new();
        q.enqueue(job(1), Some("a".into()), "faye".into());
        q.append_body(job(1), "b".into());
        q.append_body(job(1), "c".into());
        assert_eq!(q.pending_count(job(1)), 3);
        // Skip the boot prompt for brevity.
        q.on_agent_ready(job(1));
        assert_eq!(q.take_ready(job(1)).as_deref(), Some("a"));
        // After a body is taken, state goes back to WaitingForReady
        // (waiting for agent to render+settle before the next).
        assert_eq!(q.state(job(1)), Some(&PendingState::WaitingForReady));

        // Force-inject the second.
        let entry = q.entries.get_mut(&job(1)).expect("present");
        entry.ready_since = Some(Instant::now() - Duration::from_secs(2));
        assert!(q.force_inject_if_idle(job(1), Duration::from_millis(1600)));
        assert_eq!(q.take_ready(job(1)).as_deref(), Some("b"));

        let entry = q.entries.get_mut(&job(1)).expect("present");
        entry.ready_since = Some(Instant::now() - Duration::from_secs(2));
        assert!(q.force_inject_if_idle(job(1), Duration::from_millis(1600)));
        assert_eq!(q.take_ready(job(1)).as_deref(), Some("c"));
        // Last one done → Idle.
        assert_eq!(q.state(job(1)), Some(&PendingState::Idle));
    }

    #[test]
    fn force_inject_when_idle_and_ready_aged() {
        let mut q = PendingPromptQueue::new();
        q.enqueue(job(1), Some("x".into()), "faye".into());
        q.on_boot_prompt_detected(job(1));
        q.on_boot_prompt_resolved(job(1));
        let entry = q.entries.get_mut(&job(1)).expect("present");
        entry.ready_since = Some(Instant::now() - Duration::from_secs(2));

        assert!(!q.force_inject_if_idle(job(1), Duration::from_millis(500)));
        assert!(q.force_inject_if_idle(job(1), Duration::from_millis(1600)));
        assert_eq!(q.take_ready(job(1)).as_deref(), Some("x"));
    }

    #[test]
    fn force_inject_too_fresh_does_nothing() {
        let mut q = PendingPromptQueue::new();
        q.enqueue(job(1), Some("x".into()), "faye".into());
        q.on_boot_prompt_detected(job(1));
        q.on_boot_prompt_resolved(job(1));
        assert!(!q.force_inject_if_idle(job(1), Duration::from_secs(5)));
    }

    #[test]
    fn boot_inject_fires_for_alt_screen_no_boot_prompt() {
        // claude --dangerously-skip-permissions: no boot prompt ever
        // detected, so we stay in WaitingForBoot. Once aged + idle, the
        // alt-screen ready UI must deliver the boot body.
        let mut q = PendingPromptQueue::new();
        q.enqueue(job(1), Some("do the task".into()), "faye".into());
        assert_eq!(q.state(job(1)), Some(&PendingState::WaitingForBoot));
        // Too fresh (spawned < 2s ago) → no inject yet.
        assert!(!q.boot_inject_if_idle(job(1), Duration::from_millis(1600)));
        // Age the spawn past 2s.
        let entry = q.entries.get_mut(&job(1)).expect("present");
        entry.created_at = Instant::now() - Duration::from_secs(3);
        // Still drawing (idle < 1.5s) → no inject.
        assert!(!q.boot_inject_if_idle(job(1), Duration::from_millis(500)));
        // Aged + idle → inject.
        assert!(q.boot_inject_if_idle(job(1), Duration::from_millis(1600)));
        assert_eq!(q.take_ready(job(1)).as_deref(), Some("do the task"));
        assert_eq!(q.state(job(1)), Some(&PendingState::Idle));
    }

    #[test]
    fn boot_inject_never_fires_outside_waiting_for_boot() {
        // After the first body is delivered the agent may open a nested
        // editor (alt-screen). A follow-up body must NOT boot-inject into
        // it: boot_inject_if_idle only fires in WaitingForBoot.
        let mut q = PendingPromptQueue::new();
        q.enqueue(job(1), Some("first".into()), "faye".into());
        q.on_agent_ready(job(1));
        let _ = q.take_ready(job(1)); // delivered → Idle
        assert!(q.append_body(job(1), "second".into())); // → WaitingForReady
        let entry = q.entries.get_mut(&job(1)).expect("present");
        entry.created_at = Instant::now() - Duration::from_secs(3);
        assert!(!q.boot_inject_if_idle(job(1), Duration::from_millis(1600)));
    }

    #[test]
    fn cleanup_returns_dropped_when_bodies_remain() {
        let mut q = PendingPromptQueue::new();
        q.enqueue(job(1), Some("abandoned".into()), "faye".into());
        q.append_body(job(1), "also abandoned".into());
        let dropped = q.cleanup(job(1)).expect("dropped");
        assert_eq!(dropped.bodies, vec!["abandoned", "also abandoned"]);
        assert_eq!(dropped.state_at_drop, PendingState::WaitingForBoot);
    }

    #[test]
    fn cleanup_returns_none_when_queue_idle() {
        let mut q = PendingPromptQueue::new();
        q.enqueue(job(1), Some("done".into()), "faye".into());
        q.on_agent_ready(job(1));
        let _ = q.take_ready(job(1));
        // Queue now empty, agent Idle → no dropped prompts.
        assert!(q.cleanup(job(1)).is_none());
    }
}

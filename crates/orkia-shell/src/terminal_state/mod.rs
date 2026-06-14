// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Terminal state machine: per-job prompt detection without text
//! pattern matching.
//!
//! The architecture is the **passive observer** pattern. The reader
//! thread in `orkia-terminal-core::TerminalEngine` is left untouched
//! and remains the only path that touches I/O and alacritty's
//! `Term`. We subscribe a private byte stream via
//! `engine.subscribe_output()`, run our own `vte::ansi::Processor`
//! on a dedicated detector thread, and emit [`DetectorEvent`]s back
//! to the REPL through an mpsc channel.
//!
//! If a detector thread panics, only its job loses prompt detection.
//! The engine, the agent PTY, and every other agent continue to work.

mod classifier;
mod pending_prompt;
mod process_state;
mod prompt_detector;
mod vte_interceptor;
mod worker;

#[cfg(test)]
#[path = "detection_tests.rs"]
mod detection_tests_mod;

pub use classifier::PromptType;
pub use pending_prompt::{DroppedPrompt, PendingPromptQueue, PendingState};

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use orkia_shell_types::JobId;
use orkia_terminal_core::TerminalEngine;
use parking_lot::Mutex;

use worker::{DetectorThreadCtx, detector_loop, truncate};

/// Events the detector thread sends back to the REPL.
#[derive(Debug, Clone)]
pub enum DetectorEvent {
    /// A prompt was detected that needs the user's attention.
    Attention(JobAttention),
    /// The detector decided the agent is ready and handed the pending
    /// body to the injection executor to type. This is the *decision*,
    /// not the landing: the executor may still spend several seconds
    /// waiting for the agent's input box to render (confirm-retry)
    /// before the body is actually submitted. User-facing notification
    /// is deferred to [`DetectorEvent::Delivered`] so the toast/journal
    /// reflect when bytes truly land, not when typing started.
    Injected {
        job_id: JobId,
        agent_name: String,
        body: String,
    },
    /// The injection executor confirmed the body landed in the agent's
    /// input box and submitted it. Emitted by the executor thread (not
    /// the detector) at the end of delivery — this is what drives the
    /// "▸ prompt injected" toast and the journal `Tell`, so they fire
    /// when the prompt is genuinely in, ~5-6s after the decision on a
    /// slow boot.
    Delivered {
        job_id: JobId,
        agent_name: String,
        body: String,
    },
    /// The agent's child exited (PTY EOF) or its subscriber channel
    /// closed — the detector thread is exiting. `exit_code` is the code
    /// the engine reaped at EOF (`Some`), or `None` if it wasn't reapable
    /// in the grace window / the close came from job removal. Drives the
    /// exact `[N]+ Done`/`Exit N` prompt notice.
    Closed {
        job_id: JobId,
        exit_code: Option<i32>,
    },
}

#[derive(Debug, Clone)]
pub struct JobAttention {
    pub job_id: JobId,
    pub agent_name: String,
    pub confidence: f32,
    pub prompt_type: PromptType,
    pub last_line: String,
    pub has_pending_body: bool,
    pub pending_body_preview: Option<String>,
}

/// The poll interval for the detector thread: balances responsiveness
/// against `ps`/`/proc` syscall cost. With this cadence the worst-case
/// notification latency is `idle_threshold + 500ms` ≈ 1.3-2.0 s.
const TICK_INTERVAL: Duration = Duration::from_millis(500);

/// Per-job tracking state owned by the REPL side. The shared
/// [`PendingPromptQueue`] lives behind a Mutex so both the REPL and
/// the detector thread can update it; detector queries `agent_name`
/// / `has_pending` / `pending_body`, REPL drives the state machine
/// from user actions (`approve`, `reject`) and from injections.
/// Cheap-clone state-machine handle. All fields are
/// interior-mutable so methods take `&self`; this lets per-job
/// [`crate::job::lifecycle::JobLifecycleHook`] impls hold a clone
/// and call `register_agent_job` / `remove_job` from inside the
/// controller's spawn / complete dispatch.
///
/// `event_rx` is wrapped in `Arc<Mutex<Option<_>>>` so any clone
/// can pull the receiver once — subsequent calls return `None`,
/// preserving the original single-take contract.
#[derive(Clone)]
pub struct TerminalStateMachine {
    pending: Arc<Mutex<PendingPromptQueue>>,
    detectors: Arc<Mutex<std::collections::HashMap<JobId, DetectorHandle>>>,
    /// Outbound channel back to the REPL. Cloned into each spawned
    /// detector thread.
    event_tx: mpsc::Sender<DetectorEvent>,
    event_rx: Arc<Mutex<Option<mpsc::Receiver<DetectorEvent>>>>,
}

struct DetectorHandle {
    stop: Arc<std::sync::atomic::AtomicBool>,
    /// REPL-set "user is currently attached to this job" gate. While
    /// `true`, the detector still consumes bytes and updates its
    /// signals, but does NOT emit `Attention`. When the REPL flips it
    /// back to `false` (on detach), the detector clears its internal
    /// `already_notified` latch so the next tick can re-fire if the
    /// agent is still blocked on a prompt.
    muted: Arc<std::sync::atomic::AtomicBool>,
    /// Reset latch — REPL sets this to `true` to force the detector
    /// to clear `already_notified` on its next tick (used after
    /// detach so a fresh detection can fire).
    reset_notified: Arc<std::sync::atomic::AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl TerminalStateMachine {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            pending: Arc::new(Mutex::new(PendingPromptQueue::new())),
            detectors: Arc::new(Mutex::new(std::collections::HashMap::new())),
            event_tx: tx,
            event_rx: Arc::new(Mutex::new(Some(rx))),
        }
    }

    /// Take the REPL-side receiver exactly once. Subsequent calls
    /// return `None`. `&self` because the receiver lives behind
    /// `Arc<Mutex<Option<_>>>` for cheap-cloneability.
    pub fn take_event_rx(&self) -> Option<mpsc::Receiver<DetectorEvent>> {
        self.event_rx.lock().take()
    }

    /// A clone of the REPL-side event sender. Handed to the injection
    /// executor so it can emit [`DetectorEvent::Delivered`] back onto
    /// the same channel the worker drains, once a body actually lands.
    pub fn event_sender(&self) -> mpsc::Sender<DetectorEvent> {
        self.event_tx.clone()
    }

    /// Enqueue the user's intent body and spawn a detector thread for
    /// this job. The detector subscribes to the engine's output
    /// stream and starts emitting events immediately.
    pub fn register_agent_job(
        &self,
        job_id: JobId,
        agent_name: &str,
        pid: u32,
        engine: &TerminalEngine,
        body: Option<String>,
    ) {
        // Always register, even with no body — follow-up prompts via
        // `append_body` (e.g. `@faye <new task>` while faye is alive)
        // need the entry to exist.
        self.pending
            .lock()
            .enqueue(job_id, body, agent_name.to_string());

        let rx = engine.subscribe_output();
        let child_exited = engine.child_exited_handle();
        let child_exit_code = engine.child_exit_code_handle();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let muted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reset_notified = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let muted_thread = Arc::clone(&muted);
        let reset_notified_thread = Arc::clone(&reset_notified);
        let pending = Arc::clone(&self.pending);
        let event_tx = self.event_tx.clone();
        let agent_name_owned = agent_name.to_string();

        tracing::info!(
            job = job_id.0,
            agent = agent_name,
            pid,
            "terminal_state: registering job, spawning detector",
        );

        let spawn_result = thread::Builder::new()
            .name(format!("orkia-detector-{}", job_id.0))
            .spawn(move || {
                tracing::debug!(job = job_id.0, "detector thread started",);
                detector_loop(DetectorThreadCtx {
                    job_id,
                    agent_name: agent_name_owned,
                    pid,
                    rx,
                    pending,
                    event_tx,
                    stop: stop_thread,
                    muted: muted_thread,
                    reset_notified: reset_notified_thread,
                    child_exited,
                    child_exit_code,
                });
                tracing::debug!(job = job_id.0, "detector thread exiting",);
            });

        let join = match spawn_result {
            Ok(handle) => handle,
            Err(err) => {
                // OS refused the thread (RLIMIT / OOM). Log and skip:
                // missing a detector means we lose Attention events for
                // this job, but the REPL stays alive.
                tracing::error!(
                    job = job_id.0,
                    ?err,
                    "detector thread spawn failed; attention events disabled for this job",
                );
                self.pending.lock().cleanup(job_id);
                return;
            }
        };

        self.detectors.lock().insert(
            job_id,
            DetectorHandle {
                stop,
                muted,
                reset_notified,
                join: Some(join),
            },
        );
    }

    /// REPL → state machine: user just attached to `job_id`. Mutes
    /// the detector so it stops emitting `Attention` while the user
    /// is interacting directly. Returns `true` if the job was known.
    pub fn on_user_attached(&self, job_id: JobId) {
        if let Some(h) = self.detectors.lock().get(&job_id) {
            h.muted.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// REPL → state machine: user just detached from `job_id`.
    /// Un-mutes the detector and clears its `already_notified` latch
    /// so the next tick can fire a fresh `Attention` if the agent is
    /// still blocked on a prompt (claude back at the trust check,
    /// vim still waiting for keystrokes, etc.).
    pub fn on_user_detached(&self, job_id: JobId) {
        if let Some(h) = self.detectors.lock().get(&job_id) {
            h.muted.store(false, std::sync::atomic::Ordering::SeqCst);
            h.reset_notified
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// REPL → state machine: user resolved a boot prompt. The
    /// detector will re-enter detection on the next tick after the
    /// agent emits new output, transitioning ReadyForBoot →
    /// WaitingForReady → ReadyToInject when the prompt appears.
    pub fn on_prompt_resolved(&self, job_id: JobId) {
        self.pending.lock().on_boot_prompt_resolved(job_id);
        if let Some(h) = self.detectors.lock().get(&job_id) {
            // Clear `already_notified` so the next tick can fire a
            // fresh detection on the agent's *next* prompt state.
            h.reset_notified
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Query the current `PendingState` for a job. `None` when the
    /// job is unknown or has no queued body.
    pub fn pending_state(&self, job_id: JobId) -> Option<PendingState> {
        self.pending.lock().state(job_id).cloned()
    }

    /// Append a follow-up body to a running agent's queue. Returns
    /// `false` when no entry exists for `job_id` (caller should
    /// fall back to spawning a fresh job). The detector will deliver
    /// the body once the agent is idle ≥ 1.5 s + 1 s since its
    /// previous transition into `WaitingForReady`.
    pub fn append_body(&self, job_id: JobId, body: String) -> bool {
        let appended = self.pending.lock().append_body(job_id, body);
        if appended {
            // Wake the detector. After delivering a body the agent
            // re-prompts with an empty queue, and `handle_detection`
            // latches `already_notified` — which the loop only clears on
            // NEW agent bytes (an idle agent emits none) or a detach
            // reset. A follow-up `@agent <body>` flips the state to
            // `WaitingForReady` but, without this, the latched detector
            // would skip every tick and the queued body would never be
            // force-injected. Reuse the detach reset path to clear the
            // latch so the next tick re-evaluates and delivers.
            if let Some(h) = self.detectors.lock().get(&job_id) {
                h.reset_notified
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        appended
    }

    /// How many bodies are queued for delivery. `0` when idle.
    pub fn pending_count(&self, job_id: JobId) -> usize {
        self.pending.lock().pending_count(job_id)
    }

    /// REPL → state machine: job exited or was killed. Joins the
    /// detector thread and returns any pending prompt that never
    /// made it to injection.
    pub fn remove_job(&self, job_id: JobId) -> Option<DroppedPrompt> {
        // Remove + join outside the detectors lock so the joining
        // thread doesn't deadlock against a detector callback that
        // happens to grab the map.
        let removed = self.detectors.lock().remove(&job_id);
        if let Some(mut h) = removed {
            h.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(join) = h.join.take() {
                // Best-effort: don't block REPL forever if the thread
                // is stuck on a recv (it shouldn't — disconnect ends
                // recv immediately).
                let _ = join.join();
            }
        }
        self.pending.lock().cleanup(job_id)
    }

    pub fn pending_snapshot(&self) -> Vec<JobAttention> {
        // Used by the `attention list` builtin. We don't track live
        // last-detected `JobAttention` server-side (it's emitted
        // through the channel and consumed by the REPL); the builtin
        // queries the current pending state to surface what jobs
        // still have un-injected bodies + their state.
        let p = self.pending.lock();
        let mut out = Vec::new();
        for (id, agent_name, body, state) in p.entries_for_listing() {
            let body_preview = truncate(&body, 60);
            let prompt_type = match state {
                PendingState::ReadyToInject => PromptType::ShellPrompt,
                _ => PromptType::Generic,
            };
            out.push(JobAttention {
                job_id: id,
                agent_name,
                confidence: 0.0,
                prompt_type,
                last_line: format!("{state:?}"),
                has_pending_body: true,
                pending_body_preview: Some(body_preview),
            });
        }
        out
    }
}

impl Default for TerminalStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

// === PendingPromptQueue extension for the `attention list` builtin ===
//
// We add a read-only listing accessor here (instead of in the queue
// module itself) because it knits the JobAttention shape together
// with the queue contents, and lives at the same abstraction level
// as the rest of `mod.rs`.

impl PendingPromptQueue {
    pub(crate) fn entries_for_listing(&self) -> Vec<(JobId, String, String, PendingState)> {
        self.entries_internal()
            .into_iter()
            .filter(|(_, _, _, state)| *state != PendingState::Idle)
            .collect()
    }
}

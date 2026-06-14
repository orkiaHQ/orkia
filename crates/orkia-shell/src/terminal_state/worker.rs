// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use alacritty_terminal::vte::ansi::Processor;
use orkia_shell_types::JobId;
use parking_lot::Mutex;

use super::classifier::classify;
use super::pending_prompt::{PendingPromptQueue, PendingState};
use super::process_state;
use super::prompt_detector::{PromptReadiness, detect_prompt};
use super::vte_interceptor::{VteInterceptor, VteSignals};
use super::{DetectorEvent, JobAttention, PromptType, TICK_INTERVAL};

pub(super) struct DetectorThreadCtx {
    pub job_id: JobId,
    pub agent_name: String,
    pub pid: u32,
    pub rx: mpsc::Receiver<Vec<u8>>,
    pub pending: Arc<Mutex<PendingPromptQueue>>,
    pub event_tx: mpsc::Sender<DetectorEvent>,
    pub stop: Arc<std::sync::atomic::AtomicBool>,
    pub muted: Arc<std::sync::atomic::AtomicBool>,
    pub reset_notified: Arc<std::sync::atomic::AtomicBool>,
    /// Set by the engine reader on PTY EOF — the only prompt signal that
    /// the child exited (the output subscription doesn't disconnect).
    pub child_exited: Arc<std::sync::atomic::AtomicBool>,
    /// Exit code the engine reader reaped at EOF (read once `child_exited`
    /// is set). Carried on `Closed` so the prompt notice is exact.
    pub child_exit_code: Arc<Mutex<Option<i32>>>,
}

pub(super) fn detector_loop(ctx: DetectorThreadCtx) {
    let mut state = DetectorState::new(&ctx);
    detector_run_loop(ctx, &mut state);
}

struct DetectorState {
    processor: Processor<alacritty_terminal::vte::ansi::StdSyncHandler>,
    signals: VteSignals,
    last_tick: Instant,
    already_notified: bool,
    /// REF-033: cached leaf PID — avoids pgrep/ps spawns on every tick.
    cached_leaf: Option<u32>,
}

impl DetectorState {
    fn new(_ctx: &DetectorThreadCtx) -> Self {
        Self {
            processor: Processor::new(),
            signals: VteSignals::new(),
            last_tick: Instant::now(),
            already_notified: false,
            cached_leaf: None,
        }
    }
}

fn detector_run_loop(ctx: DetectorThreadCtx, s: &mut DetectorState) {
    loop {
        if ctx.stop.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        // The child exited (PTY EOF) — surface `Closed` promptly.
        // The subscriber channel never disconnects on its own, so this
        // flag is the signal.
        if ctx.child_exited.load(std::sync::atomic::Ordering::SeqCst) {
            let exit_code = *ctx.child_exit_code.lock();
            let _ = ctx.event_tx.send(DetectorEvent::Closed {
                job_id: ctx.job_id,
                exit_code,
            });
            break;
        }
        let remaining = TICK_INTERVAL.saturating_sub(s.last_tick.elapsed());
        match ctx.rx.recv_timeout(remaining) {
            Ok(bytes) => on_recv_bytes(
                &bytes,
                ctx.job_id,
                &mut s.signals,
                &mut s.processor,
                &mut s.already_notified,
            ),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                s.last_tick = Instant::now();
                let action = on_recv_timeout(TimeoutCtx {
                    job_id: ctx.job_id,
                    pid: ctx.pid,
                    agent_name: &ctx.agent_name,
                    signals: &s.signals,
                    pending: &ctx.pending,
                    event_tx: &ctx.event_tx,
                    already_notified: &mut s.already_notified,
                    muted: &ctx.muted,
                    reset_notified: &ctx.reset_notified,
                    cached_leaf: &mut s.cached_leaf,
                });
                if action == LoopAction::Continue {
                    continue;
                }
                // Note: we do NOT call `signals.reset_cycle()` here.
                // `write_count_since_reset` is the "agent has output bytes
                // at least once" flag; resetting it every tick made the
                // second tick onward always short-circuit detection.
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                on_recv_disconnected(
                    ctx.job_id,
                    &ctx.child_exited,
                    &ctx.child_exit_code,
                    &ctx.event_tx,
                );
                break;
            }
        }
    }
}

/// New bytes from the agent — update signals and reset the notified latch.
fn on_recv_bytes(
    bytes: &[u8],
    job_id: JobId,
    signals: &mut VteSignals,
    processor: &mut Processor<alacritty_terminal::vte::ansi::StdSyncHandler>,
    already_notified: &mut bool,
) {
    tracing::trace!(job = job_id.0, n = bytes.len(), "detector: bytes received");
    *already_notified = false;
    let mut handler = VteInterceptor::new(signals);
    processor.advance(&mut handler, bytes);
}

#[derive(PartialEq)]
enum LoopAction {
    Continue,
    Fall,
}

/// Bundled input for the timeout tick handler. One struct keeps the
/// function call under the 4-argument limit.
struct TimeoutCtx<'a> {
    job_id: JobId,
    pid: u32,
    agent_name: &'a str,
    signals: &'a VteSignals,
    pending: &'a Arc<Mutex<PendingPromptQueue>>,
    event_tx: &'a mpsc::Sender<DetectorEvent>,
    already_notified: &'a mut bool,
    muted: &'a Arc<std::sync::atomic::AtomicBool>,
    reset_notified: &'a Arc<std::sync::atomic::AtomicBool>,
    /// REF-033: cached leaf PID to avoid pgrep/ps spawns on each tick.
    cached_leaf: &'a mut Option<u32>,
}

/// Handle a tick timeout: run detection and dispatch the appropriate event.
/// Returns `LoopAction::Continue` when the caller should skip to the next
/// loop iteration, `Fall` to fall through to the reset-cycle note.
fn on_recv_timeout(c: TimeoutCtx<'_>) -> LoopAction {
    // REPL → us: user detached, re-check the prompt afresh.
    if c.reset_notified
        .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        *c.already_notified = false;
    }
    let is_muted = c.muted.load(std::sync::atomic::Ordering::SeqCst);
    // While attached (muted) suppress Attention toasts — but injection
    // must still fire. Skip only when nothing is queued.
    if is_muted && c.pending.lock().pending_count(c.job_id) == 0 {
        return LoopAction::Continue;
    }
    if *c.already_notified {
        return LoopAction::Continue;
    }
    let process_waiting = process_state::is_waiting_for_input_cached(c.pid, c.cached_leaf);
    let det = detect_prompt(c.signals, process_waiting);
    tracing::trace!(
        job = c.job_id.0,
        idle_ms = det.idle_duration.as_millis(),
        confidence = det.confidence,
        process_waiting,
        write_count = c.signals.write_count_since_reset,
        "detector tick"
    );
    if det.prompt_detected {
        tracing::debug!(
            job = c.job_id.0,
            confidence = det.confidence,
            "detector: prompt detected"
        );
        handle_detection(HandleDetection {
            agent_name: c.agent_name,
            job_id: c.job_id,
            signals: c.signals,
            det,
            pending: c.pending,
            event_tx: c.event_tx,
            already_notified: c.already_notified,
            is_muted,
        });
    } else {
        // No prompt detected — try idle-driven force-inject.
        try_force_inject(
            c.job_id,
            c.agent_name,
            &det,
            c.pending,
            c.event_tx,
            c.already_notified,
        );
    }
    LoopAction::Fall
}

/// No-detection path: attempt idle-driven force-injection when the classifier
/// can't pin a clean prompt (claude/codex ready UI).
fn try_force_inject(
    job_id: JobId,
    agent_name: &str,
    det: &PromptReadiness,
    pending: &Arc<Mutex<PendingPromptQueue>>,
    event_tx: &mpsc::Sender<DetectorEvent>,
    already_notified: &mut bool,
) {
    let mut p = pending.lock();
    // Two idle-driven triggers, both gated on idle≥1.5s + aging:
    //  * `force_inject_if_idle` — boot already resolved (WaitingForReady),
    //    follow-up bodies on a live agent.
    //  * `boot_inject_if_idle` — a TUI agent (claude/codex with
    //    --dangerously-skip-permissions) that never showed a boot prompt
    //    and is stuck in WaitingForBoot. This path matters when the
    //    classifier did NOT flag a prompt (`prompt_detected=false`),
    //    e.g. alt-screen confidence was discounted because the leaf
    //    wasn't seen blocked on tty-read — the complement of the
    //    AltScreenProgram branch in `handle_detection`.
    if (p.force_inject_if_idle(job_id, det.idle_duration)
        || p.boot_inject_if_idle(job_id, det.idle_duration))
        && let Some(body) = p.take_ready(job_id)
    {
        tracing::info!(
            job = job_id.0,
            idle_ms = det.idle_duration.as_millis(),
            "detector tick: force-inject fired (no detection), emitting Injected"
        );
        // The only cause of send failure is a dropped receiver at REPL
        // shutdown, but note the lost body rather than discarding it silently
        // (BUG-N10).
        if let Err(e) = event_tx.send(DetectorEvent::Injected {
            job_id,
            agent_name: agent_name.to_string(),
            body,
        }) {
            tracing::warn!(job = job_id.0, error = %e, "detector: receiver dropped; injected body lost");
        }
        *already_notified = true;
    }
}

/// Handle subscription disconnect. Races the engine's EOF reap — give
/// it a brief grace to land the exit code, then emit `Closed`.
fn on_recv_disconnected(
    job_id: JobId,
    child_exited: &Arc<std::sync::atomic::AtomicBool>,
    child_exit_code: &Arc<Mutex<Option<i32>>>,
    event_tx: &mpsc::Sender<DetectorEvent>,
) {
    // The output subscription dropped. This RACES the engine
    // reader's EOF reap: on child exit the reader reaps the code,
    // sets `child_exited`, breaks, and the dropped output sender
    // surfaces here as `Disconnected` — frequently BEFORE the
    // loop circles back to the top-of-loop `child_exited` check.
    // Emitting a bare `None` here discards the code the reader
    // already has: the prompt notice degrades to "exited" AND the
    // REPL reap's later `Some(code)` prints a SECOND `[N]+ Done`
    // (the duplicate-notice bug). So mirror the flag path — give
    // the reaper a brief grace to land its code, then carry it.
    // Both `Closed` paths now agree on the exit code.
    let mut exit_code = None;
    for _ in 0..50 {
        if child_exited.load(std::sync::atomic::Ordering::SeqCst) {
            exit_code = *child_exit_code.lock();
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = event_tx.send(DetectorEvent::Closed { job_id, exit_code });
}

/// Inputs to [`handle_detection`]. Bundled into one borrow so the
/// function stays under the argument limit and the detector loop reads
/// cleanly. `already_notified` is the loop's re-fire latch — set when a
/// quiet window has been handled so we don't re-tick until new bytes
/// arrive. `is_muted` is the "user is attached to this job" gate: it
/// suppresses `Attention` toasts but NEVER injection (the queued body
/// must still land while the user watches — that is the whole point of
/// `@agent <task>` then `attach`).
pub(super) struct HandleDetection<'a> {
    pub agent_name: &'a str,
    pub job_id: JobId,
    pub signals: &'a VteSignals,
    pub det: PromptReadiness,
    pub pending: &'a Arc<Mutex<PendingPromptQueue>>,
    pub event_tx: &'a mpsc::Sender<DetectorEvent>,
    pub already_notified: &'a mut bool,
    pub is_muted: bool,
}

/// Prompts that genuinely require a human decision (trust check,
/// password, yes/no, menu, "press enter"). These are reliably matched
/// by `classifier::classify` on the prompt text, so we hold the queued
/// body and surface an `Attention` rather than injecting into them.
fn is_interactive_prompt(p: &PromptType) -> bool {
    matches!(
        p,
        PromptType::YesNo
            | PromptType::MultipleChoice
            | PromptType::Password
            | PromptType::Continuation
    )
}

pub(super) fn handle_detection(ctx: HandleDetection<'_>) {
    let HandleDetection {
        agent_name,
        job_id,
        signals,
        det,
        pending,
        event_tx,
        already_notified,
        is_muted,
    } = ctx;
    let prompt_type = classify(
        &signals.current_line,
        &signals.recent_lines,
        signals.alt_screen,
    );
    let mut p = pending.lock();
    log_detection(job_id, &prompt_type, signals, &p, &det, is_muted);

    // 1) A real interactive prompt: hold the body, await the user.
    if is_interactive_prompt(&prompt_type) {
        hold_interactive(HoldCtx {
            p: &mut p,
            job_id,
            prompt_type: &prompt_type,
            det: &det,
            agent_name,
            last_line: &signals.current_line,
            already_notified,
            is_muted,
            event_tx,
        });
        return;
    }

    // 2) Alt-screen program. For a freshly-booted TUI agent (claude /
    //    codex with `--dangerously-skip-permissions`, so no boot prompt
    //    fires) the alt-screen IS the agent's own idle ready UI — deliver
    //    the boot body. `boot_inject_if_idle` only acts in WaitingForBoot,
    //    where the agent has no task yet and so cannot have opened a real
    //    nested editor/pager; in any later state this returns without
    //    injecting, preserving the "never inject into vim/less" guarantee.
    if prompt_type == PromptType::AltScreenProgram {
        if p.boot_inject_if_idle(job_id, det.idle_duration)
            && let Some(body) = p.take_ready(job_id)
        {
            tracing::info!(
                job = job_id.0,
                idle_ms = det.idle_duration.as_millis(),
                "detector: alt-screen boot idle, emitting Injected"
            );
            let _ = event_tx.send(DetectorEvent::Injected {
                job_id,
                agent_name: agent_name.to_string(),
                body,
            });
            *already_notified = true;
        }
        return;
    }

    // 3) ShellPrompt or Generic: deliver the queued body if ready.
    match deliver_or_hold(&mut p, job_id, &det) {
        DeliverOutcome::Hold => return,
        DeliverOutcome::Injected => {
            if let Some(body) = p.take_ready(job_id) {
                tracing::info!(
                    job = job_id.0,
                    idle_ms = det.idle_duration.as_millis(),
                    "detector: agent idle at prompt, emitting Injected"
                );
                let _ = event_tx.send(DetectorEvent::Injected {
                    job_id,
                    agent_name: agent_name.to_string(),
                    body,
                });
                *already_notified = true;
                return;
            }
        }
        DeliverOutcome::NoBody => {}
    }

    // 4) Nothing queued. Surface FYI Attention only after long idle.
    maybe_fyi_attention(MaybeAttentionCtx {
        p: &p,
        job_id,
        prompt_type: &prompt_type,
        det: &det,
        agent_name,
        last_line: &signals.current_line,
        already_notified,
        is_muted,
        event_tx,
    });
}

fn log_detection(
    job_id: JobId,
    prompt_type: &PromptType,
    signals: &VteSignals,
    p: &PendingPromptQueue,
    det: &PromptReadiness,
    is_muted: bool,
) {
    // Decision context for live debugging (angle A): one line per detection
    // showing WHY we did or didn't deliver.
    tracing::debug!(
        job = job_id.0, ?prompt_type, alt_screen = signals.alt_screen,
        state = ?p.state(job_id), has_body = p.has_pending(job_id),
        idle_ms = det.idle_duration.as_millis(), confidence = det.confidence,
        is_muted, "handle_detection: classified",
    );
}

struct HoldCtx<'a> {
    p: &'a mut PendingPromptQueue,
    job_id: JobId,
    prompt_type: &'a PromptType,
    det: &'a PromptReadiness,
    agent_name: &'a str,
    last_line: &'a str,
    already_notified: &'a mut bool,
    is_muted: bool,
    event_tx: &'a mpsc::Sender<DetectorEvent>,
}

/// Handle a detected interactive prompt: hold the queued body and
/// (when not muted) surface an `Attention` event.
fn hold_interactive(c: HoldCtx<'_>) {
    if c.p.has_pending(c.job_id) {
        c.p.on_boot_prompt_detected(c.job_id);
    }
    *c.already_notified = true;
    if !c.is_muted {
        let attention = build_attention(BuildAttention {
            p: c.p,
            job_id: c.job_id,
            prompt_type: c.prompt_type,
            det: c.det,
            agent_name: c.agent_name,
            last_line: c.last_line,
        });
        let _ = c.event_tx.send(DetectorEvent::Attention(attention));
    }
}

#[derive(PartialEq)]
enum DeliverOutcome {
    Injected,
    Hold,
    NoBody,
}

/// Advance the pending-state machine and decide whether to inject.
/// Returns `Injected` when `take_ready` should be called and sent,
/// `Hold` when the caller should return without injecting or notifying,
/// `NoBody` when there is nothing queued and the FYI path should run.
fn deliver_or_hold(
    p: &mut PendingPromptQueue,
    job_id: JobId,
    det: &PromptReadiness,
) -> DeliverOutcome {
    match p.state(job_id) {
        Some(PendingState::WaitingForBoot) => p.on_agent_ready(job_id),
        Some(PendingState::WaitingForReady) => {
            if !p.force_inject_if_idle(job_id, det.idle_duration) {
                // A body is queued but its idle/ready timers haven't
                // elapsed yet. Do NOT latch `already_notified` here: an
                // idle agent emits no further bytes to clear the latch,
                // so latching would strand the body forever (the
                // follow-up `@agent <body>` delivery bug). Returning
                // without latching keeps the loop polling.
                return DeliverOutcome::Hold;
            }
        }
        _ => {}
    }
    if p.has_pending(job_id) {
        DeliverOutcome::Injected
    } else {
        DeliverOutcome::NoBody
    }
}

struct MaybeAttentionCtx<'a> {
    p: &'a PendingPromptQueue,
    job_id: JobId,
    prompt_type: &'a PromptType,
    det: &'a PromptReadiness,
    agent_name: &'a str,
    last_line: &'a str,
    already_notified: &'a mut bool,
    is_muted: bool,
    event_tx: &'a mpsc::Sender<DetectorEvent>,
}

/// Emit a low-priority FYI `Attention` after a long idle, if not muted.
fn maybe_fyi_attention(c: MaybeAttentionCtx<'_>) {
    if !c.is_muted && c.det.idle_duration >= Duration::from_secs(10) {
        let attention = build_attention(BuildAttention {
            p: c.p,
            job_id: c.job_id,
            prompt_type: c.prompt_type,
            det: c.det,
            agent_name: c.agent_name,
            last_line: c.last_line,
        });
        let _ = c.event_tx.send(DetectorEvent::Attention(attention));
    }
    *c.already_notified = true;
}

struct BuildAttention<'a> {
    p: &'a PendingPromptQueue,
    job_id: JobId,
    prompt_type: &'a PromptType,
    det: &'a PromptReadiness,
    agent_name: &'a str,
    last_line: &'a str,
}

fn build_attention(a: BuildAttention<'_>) -> JobAttention {
    JobAttention {
        job_id: a.job_id,
        agent_name: a.agent_name.to_string(),
        confidence: a.det.confidence,
        prompt_type: a.prompt_type.clone(),
        last_line: a.last_line.to_string(),
        has_pending_body: a.p.has_pending(a.job_id),
        pending_body_preview: a.p.pending_body(a.job_id).map(|b| truncate(b, 60)),
    }
}

pub(super) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

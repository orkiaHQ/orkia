// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

#[cfg(test)]
mod detection_tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::time::Duration;

    use orkia_shell_types::JobId;
    use parking_lot::Mutex;

    use super::super::prompt_detector::PromptReadiness;
    use super::super::vte_interceptor::VteSignals;
    use super::super::worker::{
        DetectorThreadCtx, HandleDetection, detector_loop, handle_detection,
    };
    use super::super::{DetectorEvent, PendingPromptQueue, PendingState};

    fn det(idle_ms: u64) -> PromptReadiness {
        PromptReadiness {
            prompt_detected: true,
            confidence: 0.8,
            idle_duration: Duration::from_millis(idle_ms),
        }
    }

    fn signals_with_line(line: &str) -> VteSignals {
        let mut s = VteSignals::new();
        s.current_line = line.to_string();
        s
    }

    fn boot_queue() -> Arc<Mutex<PendingPromptQueue>> {
        let q = Arc::new(Mutex::new(PendingPromptQueue::new()));
        q.lock()
            .enqueue(JobId(1), Some("say hellooo".into()), "faye".into());
        q
    }

    /// Run one `handle_detection` pass; returns whether it latched the
    /// re-fire flag and the event receiver. Most detections latch, but a
    /// follow-up body that isn't ready to inject yet deliberately does
    /// NOT (so the loop keeps polling) — see
    /// `fresh_waiting_for_ready_is_not_injected_yet`.
    fn run(
        pending: &Arc<Mutex<PendingPromptQueue>>,
        signals: &VteSignals,
        is_muted: bool,
    ) -> (bool, mpsc::Receiver<DetectorEvent>) {
        let (tx, rx) = mpsc::channel();
        let mut already = false;
        handle_detection(HandleDetection {
            agent_name: "faye",
            job_id: JobId(1),
            signals,
            det: det(2000),
            pending,
            event_tx: &tx,
            already_notified: &mut already,
            is_muted,
        });
        (already, rx)
    }

    /// Bug 1: a fresh agent (WaitingForBoot) idle at its plain input box
    /// classifies as `Generic` — not a clean `ShellPrompt`. The queued
    /// initial body must still be delivered, NOT mistaken for a boot
    /// prompt that needs `approve`. This is the core of why
    /// `@faye say hellooo` failed to inject.
    #[test]
    fn generic_ready_box_delivers_initial_body() {
        let pending = boot_queue();
        let (_, rx) = run(
            &pending,
            &signals_with_line("│ >                  │"),
            false,
        );
        match rx.try_recv() {
            Ok(DetectorEvent::Injected { body, .. }) => assert_eq!(body, "say hellooo"),
            other => panic!("expected Injected, got {other:?}"),
        }
    }

    /// Bug 1b: while the user is attached (muted) injection must STILL
    /// fire — the mute only suppresses `Attention` toasts. `@faye <task>`
    /// then an immediate `attach` must still type the body in.
    #[test]
    fn muted_still_injects_pending_body() {
        let pending = boot_queue();
        let (_, rx) = run(&pending, &signals_with_line("waiting for input"), true);
        match rx.try_recv() {
            Ok(DetectorEvent::Injected { body, .. }) => assert_eq!(body, "say hellooo"),
            other => panic!("muted attach must still inject, got {other:?}"),
        }
    }

    /// A genuine interactive prompt (trust check) is held, not injected
    /// into; the user is notified so they can `approve`.
    #[test]
    fn interactive_prompt_holds_body_and_notifies() {
        let pending = boot_queue();
        let (_, rx) = run(
            &pending,
            &signals_with_line("Trust this folder? [y/N]"),
            false,
        );
        match rx.try_recv() {
            Ok(DetectorEvent::Attention(a)) => assert!(a.has_pending_body),
            other => panic!("expected Attention, got {other:?}"),
        }
        assert_eq!(
            pending.lock().state(JobId(1)),
            Some(&PendingState::WaitingForApproval)
        );
    }

    /// The same interactive prompt while attached (muted): the queue
    /// still transitions to WaitingForApproval, but NO `Attention` toast
    /// is emitted (the user sees the prompt directly).
    #[test]
    fn interactive_prompt_muted_suppresses_attention() {
        let pending = boot_queue();
        let (_, rx) = run(&pending, &signals_with_line("Password:"), true);
        assert!(
            rx.try_recv().is_err(),
            "muted attach must not emit Attention"
        );
        assert_eq!(
            pending.lock().state(JobId(1)),
            Some(&PendingState::WaitingForApproval)
        );
    }

    /// Follow-up delivery (WaitingForReady) stays idle-gated: a freshly
    /// queued body must NOT inject until the agent has settled, so we
    /// never type over a still-rendering previous response.
    #[test]
    fn fresh_waiting_for_ready_is_not_injected_yet() {
        let pending = Arc::new(Mutex::new(PendingPromptQueue::new()));
        {
            let mut q = pending.lock();
            q.enqueue(JobId(1), Some("first".into()), "faye".into());
            q.on_agent_ready(JobId(1));
            let _ = q.take_ready(JobId(1)); // "first" delivered -> Idle
            q.append_body(JobId(1), "second".into()); // -> WaitingForReady (ready_since = now)
        }
        let (latched, rx) = run(&pending, &signals_with_line("ready"), false);
        assert!(
            rx.try_recv().is_err(),
            "a just-queued follow-up must wait for the agent to settle"
        );
        // ...but it must NOT latch the re-fire flag: an idle agent emits
        // no further bytes, so latching here would strand the body
        // forever. The loop must keep polling until the timers elapse.
        assert!(
            !latched,
            "a not-yet-ready follow-up must keep the detector polling, not latch"
        );
    }

    /// Regression: the duplicate `[N]+ Done` notice (confirmed on real claude,
    /// attach → Ctrl-C exit). The engine reader reaps the exit code at PTY EOF
    /// and sets `child_exited`, but the output subscription drops at almost the
    /// same instant — so the detector's blocked `recv_timeout` returns
    /// `Disconnected` BEFORE it can re-check the flag. The disconnect arm used
    /// to hard-code `None`, which lost the reaped code: the prompt notice
    /// degraded to "exited" with no dedup latch, and the REPL reap's later
    /// `Some(0)` printed a SECOND `[N]+ Done`. The arm must carry the reaped
    /// code instead, so both `Closed` paths agree and the notice is single.
    #[test]
    fn disconnect_after_eof_reap_carries_the_code_not_none() {
        let (byte_tx, byte_rx) = mpsc::channel::<Vec<u8>>();
        let (ev_tx, ev_rx) = mpsc::channel::<DetectorEvent>();
        let child_exited = Arc::new(AtomicBool::new(false));
        let child_exit_code = Arc::new(Mutex::new(None));
        let ctx = DetectorThreadCtx {
            job_id: JobId(1),
            agent_name: "faye".into(),
            pid: std::process::id(),
            rx: byte_rx,
            pending: Arc::new(Mutex::new(PendingPromptQueue::new())),
            event_tx: ev_tx,
            stop: Arc::new(AtomicBool::new(false)),
            muted: Arc::new(AtomicBool::new(false)),
            reset_notified: Arc::new(AtomicBool::new(false)),
            child_exited: Arc::clone(&child_exited),
            child_exit_code: Arc::clone(&child_exit_code),
        };
        let handle = std::thread::spawn(move || detector_loop(ctx));

        // Let the loop pass its top-of-loop flag check (sees false) and block
        // in `recv_timeout`, BEFORE the EOF reap lands — this is what forces
        // the disconnect arm rather than the flag path.
        std::thread::sleep(Duration::from_millis(60));

        // Engine-reader EOF reap, in engine.rs order: code first, then the
        // flag, then the output sender drops (→ Disconnected for the detector).
        *child_exit_code.lock() = Some(0);
        child_exited.store(true, std::sync::atomic::Ordering::SeqCst);
        drop(byte_tx);

        let ev = match ev_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(e) => e,
            Err(e) => panic!("detector never emitted Closed: {e:?}"),
        };
        match ev {
            DetectorEvent::Closed { exit_code, .. } => assert_eq!(
                exit_code,
                Some(0),
                "disconnect-on-exit must carry the reaped code, else `[N]+ Done` doubles",
            ),
            other => panic!("expected Closed, got {other:?}"),
        }
        let _ = handle.join();
    }
}

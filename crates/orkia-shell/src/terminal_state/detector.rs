// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Structural prompt detection. **Zero text matching.**
//!
//! Three signals are combined into a confidence score:
//!
//! 1. *Write stopped* — the agent emitted bytes recently and has now
//!    been silent past an idle threshold. If `write_count == 0` we
//!    short-circuit: an agent that has not written anything is not at
//!    a prompt, it is starting up.
//! 2. *Cursor positioned* — the most recent VTE event was a cursor
//!    move (`CSI H/f`), not a printable char. Cosmetic signal: TUIs
//!    re-position the cursor in every frame, so its discriminative
//!    power is low. Weighted accordingly.
//! 3. *Process in read()* — the leaf descendant of the agent's PID
//!    is sleeping on a TTY read syscall (`tty_read` on Linux,
//!    `S`/`I` state on macOS). OS-level fact, the strongest signal.
//!
//! Dual idle threshold: `800ms` when `process_waiting >= 0.9` (OS
//! signal is strong, we don't need to wait for confirmation),
//! `1500ms` otherwise.

use std::time::Duration;

use super::vte_interceptor::VteSignals;

#[derive(Debug, Clone)]
pub struct DetectionResult {
    pub prompt_detected: bool,
    pub confidence: f32,
    pub idle_duration: Duration,
}

const IDLE_THRESHOLD_STRONG: Duration = Duration::from_millis(800);
const IDLE_THRESHOLD_DEFAULT: Duration = Duration::from_millis(1500);
const CONFIDENCE_THRESHOLD: f32 = 0.55;

pub fn detect(vte: &VteSignals, process_waiting: f32) -> DetectionResult {
    let idle = vte.idle_duration();
    let no_detection = DetectionResult {
        prompt_detected: false,
        confidence: 0.0,
        idle_duration: idle,
    };

    // Agent has not written anything in this cycle yet — could be
    // about to render the next frame. Not a prompt.
    if vte.write_count_since_reset == 0 {
        return no_detection;
    }

    // Idle threshold depends on the OS-level confidence: when the
    // kernel is sure the leaf is blocked on tty input, 800ms is
    // enough; otherwise wait for 1500ms to filter out rendering
    // pauses.
    let idle_required = if process_waiting >= 0.9 {
        IDLE_THRESHOLD_STRONG
    } else {
        IDLE_THRESHOLD_DEFAULT
    };
    if idle < idle_required {
        return no_detection;
    }

    let cursor_ready = vte.cursor_positioned_after_text;
    let process_in_read = process_waiting >= 0.7;

    // Base weight for the write-stopped condition (already required).
    let mut confidence: f32 = 0.20;

    // Longer idle = higher confidence.
    if idle >= Duration::from_secs(2) {
        confidence += 0.10;
    }
    if idle >= Duration::from_secs(5) {
        confidence += 0.05;
    }
    if idle >= Duration::from_secs(10) {
        confidence += 0.05;
    }

    // Cursor positioning after text. Cosmetic, weight reduced from
    // the cursor in every render frame and the signal is noisier
    // than the others.
    if cursor_ready {
        confidence += 0.10;
    }

    // OS-level confirmation: the leaf is blocked on input. Strongest
    // discriminating signal we have.
    if process_in_read {
        confidence += 0.30;
        if process_waiting >= 0.9 {
            confidence += 0.05;
        }
    }

    // Alt-screen with no read() = editor/pager rendering, not a
    // prompt. Discount heavily so vim's idle screen doesn't show as
    // "waiting for approval".
    if vte.alt_screen && !process_in_read {
        confidence *= 0.3;
    }

    let confidence = confidence.min(0.99);

    DetectionResult {
        prompt_detected: confidence >= CONFIDENCE_THRESHOLD,
        confidence,
        idle_duration: idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    fn signals_with_writes(n: u64) -> VteSignals {
        let mut s = VteSignals::new();
        s.write_count_since_reset = n;
        s
    }

    #[test]
    fn no_writes_means_no_prompt() {
        let s = signals_with_writes(0);
        let r = detect(&s, 1.0);
        assert!(!r.prompt_detected);
        assert_eq!(r.confidence, 0.0);
    }

    #[test]
    fn too_soon_after_write_is_not_a_prompt() {
        let s = signals_with_writes(10);
        let r = detect(&s, 1.0);
        assert!(!r.prompt_detected, "idle <800ms must not fire");
    }

    #[test]
    fn strong_os_signal_lowers_idle_threshold() {
        let mut s = signals_with_writes(10);
        // Backdate last_write so idle is ~900ms.
        s.last_write_at = std::time::Instant::now() - Duration::from_millis(900);
        let strong = detect(&s, 1.0);
        let weak = detect(&s, 0.6);
        assert!(
            strong.prompt_detected,
            "strong OS signal must fire at 900ms"
        );
        assert!(
            !weak.prompt_detected,
            "weak OS signal must NOT fire at 900ms"
        );
    }

    #[test]
    fn altscreen_without_read_is_discounted() {
        let mut s = signals_with_writes(10);
        s.last_write_at = std::time::Instant::now() - Duration::from_secs(2);
        s.alt_screen = true;
        let r = detect(&s, 0.0);
        assert!(
            !r.prompt_detected,
            "alt-screen vim must not surface a prompt"
        );
    }

    #[test]
    fn idle_growth_increases_confidence() {
        let mut s = signals_with_writes(10);
        s.last_write_at = std::time::Instant::now() - Duration::from_secs(11);
        s.cursor_positioned_after_text = true;
        let r = detect(&s, 1.0);
        assert!(r.prompt_detected);
        assert!(
            r.confidence >= 0.75,
            "long idle should push confidence high: {}",
            r.confidence
        );
    }

    #[test]
    fn cursor_signal_alone_below_threshold() {
        let mut s = signals_with_writes(10);
        s.last_write_at = std::time::Instant::now() - Duration::from_millis(1600);
        s.cursor_positioned_after_text = true;
        // No process_in_read signal.
        let r = detect(&s, 0.4);
        assert!(!r.prompt_detected, "cursor + idle alone is not enough");
    }

    #[test]
    fn sleep_then_detect_strong_signal() {
        // Integration-style: real sleep so last_write_at ages naturally.
        let s = signals_with_writes(1);
        sleep(Duration::from_millis(900));
        let r = detect(&s, 1.0);
        assert!(r.prompt_detected, "real idle + strong OS = detect");
    }
}

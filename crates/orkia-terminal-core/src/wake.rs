// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Event-driven render wake. Replaces the old `Arc<AtomicBool>` + timer-poll:
//! producers (extractor, pty reader, input bar) signal; `ContentView` awaits.
//! Unbounded so `notify()` never blocks; the receiver coalesces bursts into a

use async_channel::{Receiver, Sender, unbounded};

/// Sender half — cloned to every repaint producer.
#[derive(Clone)]
pub struct Wake(Sender<()>);

/// Receiver half — held by `ContentView`'s redraw task.
pub struct WakeRx(Receiver<()>);

pub fn wake_pair() -> (Wake, WakeRx) {
    let (tx, rx) = unbounded();
    (Wake(tx), WakeRx(rx))
}

impl Wake {
    /// Request a repaint. Non-blocking; never fails meaningfully (unbounded;
    /// a closed channel just means the view is gone).
    pub fn notify(&self) {
        let _ = self.0.try_send(());
    }
}

impl WakeRx {
    /// Await the next repaint request, then drain any coalesced extras so a
    /// burst (publish + keystroke + …) collapses into one frame. `None` once
    /// all senders are dropped (shutdown).
    pub async fn next(&self) -> Option<()> {
        self.0.recv().await.ok()?;
        while self.0.try_recv().is_ok() {}
        Some(())
    }

    /// Synchronous sibling of [`next`](Self::next) for non-async consumers
    /// (the perf harness, tests). Same coalescing semantics.
    pub fn recv_blocking(&self) -> Option<()> {
        self.0.recv_blocking().ok()?;
        while self.0.try_recv().is_ok() {}
        Some(())
    }

    /// Non-blocking drain. Returns `true` if at least one repaint request was
    /// pending (and consumes any coalesced extras), `false` if the queue was
    /// empty. Used by the attached-mode renderer to decide whether to redraw
    /// without parking the thread.
    pub fn try_drain(&self) -> bool {
        if self.0.try_recv().is_err() {
            return false;
        }
        while self.0.try_recv().is_ok() {}
        true
    }
}

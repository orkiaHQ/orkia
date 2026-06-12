// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `ScreenSnapshot` and `ScreenView` — the lock-free read path on top
//! of the screen-mode (`Inline` / `AltScreen`) alacritty `Term` grid.
//!
//! Pattern is identical to `blocks::spawn_extractor`'s
//! `Arc<Mutex<Snapshot>>` published slot, but over an
//! `arc_swap::ArcSwap` so render-time reads are a single atomic load
//! with no spin/wait. The mutator is the engine reader thread, which
//! publishes a fresh `ScreenSnapshot` after each `proc.advance(...)`
//! in screen mode at most every `ScreenExtractConfig::min_publish`
//! (default 16 ms / 60 Hz), and on PTY EOF unconditionally.

use std::sync::Arc;

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use arc_swap::ArcSwap;

use crate::state::DisplayMode;
use crate::wake::Wake;

/// Immutable snapshot of the visible alacritty grid published by the
/// engine reader thread. Cloned by every read-side consumer for
/// one-shot rendering. Same role as `blocks::Snapshot` but for screen
/// mode (`InlineFull` / `AltScreenFull`).
#[derive(Clone, Debug)]
pub struct ScreenSnapshot {
    /// Monotonic counter, incremented by the publisher on every store.
    /// Consumers compare against the last value they observed to
    /// short-circuit "nothing changed since last poll" without diffing
    /// the bytes.
    pub generation: u64,
    /// Display mode at the moment of publication. Lets consumers
    /// detect "engine drifted into BlockView; this snapshot is not
    /// meaningful for the screen path" without locking `SharedState`
    /// separately.
    pub display: DisplayMode,
    /// Pre-rendered ANSI bytes of the visible viewport — the output of
    /// `render_snapshot::render_visible`. The reader thread renders
    /// into a fresh `String`/`Vec<u8>`, freezes into `Arc<[u8]>`, and
    /// stores via `ArcSwap`. Consumers detach the bytes from the
    /// guard with one pointer copy.
    ///
    /// `render_visible` terminates with `\x1b[{row};{col}H` so cursor
    /// position is implicitly carried — no separate `cursor` field.
    pub ansi: Arc<[u8]>,
    /// Geometry at publish time. Lets consumers self-validate against
    /// host-tty geometry after a resize race.
    pub cols: u16,
    pub rows: u16,
}

impl ScreenSnapshot {
    /// The initial sentinel snapshot published at engine construction
    /// time, before any PTY bytes have been read. `generation = 0`,
    /// empty ANSI body, mode borrowed from the engine's initial
    /// `DisplayMode` (`InlineFull` by default per
    /// `StateMachine::new`).
    pub(crate) fn empty(cols: u16, rows: u16) -> Self {
        Self {
            generation: 0,
            display: DisplayMode::InlineFull,
            ansi: Arc::<[u8]>::from(Vec::<u8>::new()),
            cols,
            rows,
        }
    }

    /// Build a snapshot from a live `Term`. Called by the engine
    /// reader thread inside its publish step. The caller is
    /// responsible for holding the grid lock for the duration of the
    /// call; `render_visible` walks `lines × cols` cells.
    ///
    /// `generation` is supplied by the caller (the reader's local
    /// counter); `display` is supplied by the caller (already known
    /// from the most recent state-machine read on the hot path).
    pub(crate) fn from_term<T: EventListener>(
        term: &Term<T>,
        generation: u64,
        display: DisplayMode,
    ) -> Self {
        let grid = term.grid();
        let cols = grid.columns() as u16;
        let rows = grid.screen_lines() as u16;
        let body = crate::render_snapshot::render_visible(term).into_bytes();
        Self {
            generation,
            display,
            ansi: Arc::<[u8]>::from(body),
            cols,
            rows,
        }
    }
}

/// Cheap, clone-friendly read handle for the latest published
/// `ScreenSnapshot`. Cloning a `ScreenView` clones two `Arc`s — no
/// allocation, no lock. The only mutator is the engine reader thread.
///
/// PR1: this type is added alongside the legacy `engine.screen()` /
/// `engine.render_visible_snapshot()` accessors. PR2 onward migrates
/// consumers off the legacy path.
#[derive(Clone)]
pub struct ScreenView {
    inner: Arc<ArcSwap<ScreenSnapshot>>,
    /// Wake-on-publish — the engine reader thread fires `wake.notify()`
    /// after every snapshot store, so consumers driven by `WakeRx`
    /// see the new generation on the next wake.
    pub wake: Wake,
}

impl ScreenView {
    /// Construct a `ScreenView` over an existing publish slot. Used
    /// by the engine constructors; not part of the public API surface.
    pub(crate) fn new(inner: Arc<ArcSwap<ScreenSnapshot>>, wake: Wake) -> Self {
        Self { inner, wake }
    }

    /// Lock-free read of the current snapshot. Returns a guard whose
    /// `Deref` yields `&Arc<ScreenSnapshot>`. Cheap (one atomic load).
    /// To detach the bytes from the guard (so the consumer can release
    /// the guard before slow I/O), call `snapshot()` or
    /// `snapshot_bytes()`.
    pub fn load(&self) -> arc_swap::Guard<Arc<ScreenSnapshot>> {
        self.inner.load()
    }

    /// Clone the current `Arc<ScreenSnapshot>` so the caller can hold
    /// it across `stdout.write_all` or other slow I/O without keeping
    /// the `ArcSwap` guard alive (which would block a concurrent
    /// publisher's store on consumer-side memory reclamation).
    pub fn snapshot(&self) -> Arc<ScreenSnapshot> {
        Arc::clone(&self.inner.load())
    }

    /// Copy out the current ANSI body. Convenience for the attach
    /// replay path which today calls `render_visible_snapshot() ->
    /// Vec<u8>`. The copy is the same one that
    /// `render_visible_snapshot()` performs today.
    pub fn snapshot_bytes(&self) -> Vec<u8> {
        self.inner.load().ansi.to_vec()
    }
}

/// Reader-thread publish slot + bookkeeping. Owned by the engine
/// reader thread (never shared as a mutator). The `Arc<ArcSwap>` is
/// cloned into every `ScreenView` handed out by
/// `TerminalEngine::screen_view`.
pub(crate) struct ScreenPublisher {
    pub(crate) inner: Arc<ArcSwap<ScreenSnapshot>>,
    pub(crate) generation: u64,
    pub(crate) last_publish: std::time::Instant,
    pub(crate) min_publish: std::time::Duration,
}

impl ScreenPublisher {
    pub(crate) fn new(cols: u16, rows: u16, min_publish: std::time::Duration) -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(ScreenSnapshot::empty(cols, rows))),
            generation: 0,
            // `Instant::now() - min_publish` would let the first
            // publish fire immediately; cleaner is `Instant::now()`
            // so the first actual publish happens after one full
            // budget window. The reader's EOF branch publishes
            // unconditionally regardless of the budget, so a
            // short-lived child still produces one snapshot.
            last_publish: std::time::Instant::now(),
            min_publish,
        }
    }

    /// Return a `ScreenView` clone pointing at this publisher's
    /// `ArcSwap`. The same `Wake` the engine uses for repaint is
    /// passed through so view consumers can subscribe to the same
    /// coalesced notify stream.
    ///
    /// Test-only today: the engine constructs `ScreenView` directly
    /// from its own `screen_snapshot` field (so the publisher can be
    /// moved into the reader thread without lending out a borrow).
    /// PR2+ may grow callers — keep it `pub(crate)` regardless.
    #[cfg(test)]
    pub(crate) fn view(&self, wake: Wake) -> ScreenView {
        ScreenView::new(Arc::clone(&self.inner), wake)
    }

    /// If at least `min_publish` has elapsed since the last store,
    /// render `term` to a fresh `ScreenSnapshot` and store it.
    /// Returns `true` if a store happened (so the caller can
    /// `wake.notify()` once after).
    ///
    /// `display` is passed in (rather than re-read from the state
    /// machine) because the reader hot path already loaded it for
    /// the screen-mode branch.
    pub(crate) fn maybe_publish<T: EventListener>(
        &mut self,
        term: &Term<T>,
        display: DisplayMode,
        now: std::time::Instant,
    ) -> bool {
        if now.saturating_duration_since(self.last_publish) < self.min_publish {
            return false;
        }
        self.publish(term, display, now);
        true
    }

    /// Unconditional publish — used on EOF and on the first byte of a
    /// session so the snapshot reflects the latest state regardless
    /// of the time budget.
    pub(crate) fn publish<T: EventListener>(
        &mut self,
        term: &Term<T>,
        display: DisplayMode,
        now: std::time::Instant,
    ) {
        self.generation = self.generation.wrapping_add(1);
        let snap = ScreenSnapshot::from_term(term, self.generation, display);
        self.inner.store(Arc::new(snap));
        self.last_publish = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_snapshot_has_generation_zero_and_empty_ansi() {
        let snap = ScreenSnapshot::empty(80, 24);
        assert_eq!(snap.generation, 0);
        assert_eq!(snap.cols, 80);
        assert_eq!(snap.rows, 24);
        assert!(snap.ansi.is_empty());
        assert_eq!(snap.display, DisplayMode::InlineFull);
    }

    #[test]
    fn publisher_initial_state_is_the_empty_snapshot() {
        let pub_ = ScreenPublisher::new(80, 24, std::time::Duration::from_millis(16));
        let snap = pub_.inner.load();
        assert_eq!(snap.generation, 0);
        assert_eq!(snap.cols, 80);
        assert_eq!(snap.rows, 24);
    }

    #[test]
    fn view_clone_is_lock_free_share() {
        // Two views over the same publisher should observe the same
        // snapshot pointer. This is the property that lets multiple
        // consumers read in parallel without coordination.
        let pub_ = ScreenPublisher::new(80, 24, std::time::Duration::from_millis(16));
        let (wake, _wake_rx) = crate::wake::wake_pair();
        let v1 = pub_.view(wake.clone());
        let v2 = pub_.view(wake);
        let a = v1.snapshot();
        let b = v2.snapshot();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(a.generation, 0);
        assert_eq!(b.generation, 0);
    }
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! 2-axis pane state machine (v3). Core invariant: raw/alt signals are only
//! honoured while a command runs (between OSC133 C and D); at the prompt the
//! state is forced to Cooked/BlockView. Includes a manual override so the
//! user is never stuck if auto-detection misses (e.g. sudo, ssh password).

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

const DEBOUNCE: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Cooked,
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    BlockView,
    InlineFull,
    AltScreenFull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneState {
    pub input: InputMode,
    pub display: DisplayMode,
}

impl PaneState {
    const DEFAULT: PaneState = PaneState {
        input: InputMode::Cooked,
        display: DisplayMode::BlockView,
    };
}

/// Manual override (Ctrl+\): the safety net.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Manual {
    None,
    ForceRaw,
    ForceCooked,
}

pub struct StateMachine {
    /// True between OSC133 C and D — the gate for every other signal.
    capturing: bool,
    /// The engine hosts a single, long-lived full-screen program (an
    /// agent TUI: claude / codex / gemini) rather than a shell that
    /// runs a sequence of commands. Such a program owns the screen for
    /// its entire life — there is no "between commands" prompt to fall
    /// back to, and it never emits the OSC-133 C/D markers the brush
    /// shell uses to bracket commands. In this mode the display is
    /// never `BlockView`, so the engine reader always advances the
    /// alacritty grid and `render_visible_snapshot` can reconstruct the
    /// screen on (re-)attach. See `state.rs` core-invariant note.
    persistent: bool,
    alt: bool,
    bracketed: bool,
    cursor_hidden: bool,
    app_cursor: bool,
    manual: Manual,
    state: PaneState,
    pending: Option<(PaneState, Instant)>,
    last_transition: Instant,
}

pub type SharedState = Arc<Mutex<StateMachine>>;

impl StateMachine {
    pub fn new() -> Self {
        Self {
            capturing: false,
            persistent: false,
            alt: false,
            bracketed: false,
            cursor_hidden: false,
            app_cursor: false,
            manual: Manual::None,
            state: PaneState::DEFAULT,
            pending: None,
            last_transition: Instant::now(),
        }
    }

    /// State machine for an engine hosting a persistent full-screen
    /// program (an agent TUI). The display starts in `InlineFull`
    /// (the program owns the screen immediately) and never returns to
    /// `BlockView` for the process's lifetime; OSC-133 C/D markers are
    /// ignored. See the [`Self::persistent`] field doc.
    pub fn new_persistent() -> Self {
        Self {
            capturing: true,
            persistent: true,
            state: PaneState {
                input: InputMode::Raw,
                display: DisplayMode::InlineFull,
            },
            ..Self::new()
        }
    }

    pub fn state(&self) -> PaneState {
        self.state
    }

    fn target(&self) -> PaneState {
        match self.manual {
            Manual::ForceCooked => return PaneState::DEFAULT,
            Manual::ForceRaw => {
                return PaneState {
                    input: InputMode::Raw,
                    display: DisplayMode::InlineFull,
                };
            }
            Manual::None => {}
        }
        // A persistent full-screen program owns the screen for its
        // whole life — never fall back to BlockView (which would stop
        // the engine reader advancing the grid). Honour alt-screen;
        // otherwise stay InlineFull regardless of OSC-133 / capturing.
        if self.persistent {
            return PaneState {
                input: InputMode::Raw,
                display: if self.alt {
                    DisplayMode::AltScreenFull
                } else {
                    DisplayMode::InlineFull
                },
            };
        }
        // Invariant: no command running -> always Cooked/BlockView.
        if !self.capturing {
            return PaneState::DEFAULT;
        }
        if self.alt {
            return PaneState {
                input: InputMode::Raw,
                display: DisplayMode::AltScreenFull,
            };
        }
        if self.bracketed || self.cursor_hidden || self.app_cursor {
            return PaneState {
                input: InputMode::Raw,
                display: DisplayMode::InlineFull,
            };
        }
        PaneState::DEFAULT
    }

    /// Recompute. Returning to default is immediate; entering a richer mode
    /// is debounced to avoid flicker (git log -> pager -> back, menus).
    fn recompute(&mut self) {
        let target = self.target();
        if target == self.state {
            self.pending = None;
            return;
        }
        if target == PaneState::DEFAULT {
            self.apply(target);
            return;
        }
        if self.last_transition.elapsed() >= DEBOUNCE {
            self.apply(target);
        } else {
            self.pending = Some((target, Instant::now()));
        }
    }

    fn apply(&mut self, s: PaneState) {
        self.state = s;
        self.pending = None;
        self.last_transition = Instant::now();
    }

    /// Apply a deferred transition once the debounce window elapsed.
    pub fn tick(&mut self) {
        if let Some((p, at)) = self.pending
            && at.elapsed() >= DEBOUNCE
        {
            self.apply(p);
        }
    }

    pub fn set_capturing(&mut self, on: bool) {
        // Persistent-program engines (agent TUIs) are "capturing" for
        // the process's entire life. A stray OSC-133 C/D from the
        // agent — or from a tool it shells out to — must not gate the
        // display mode or clear the alt/bracketed/cursor signals.
        if self.persistent {
            return;
        }
        if self.capturing == on {
            return;
        }
        self.capturing = on;
        if !on {
            // Command ended: clear program signals + manual override.
            self.alt = false;
            self.bracketed = false;
            self.cursor_hidden = false;
            self.app_cursor = false;
            self.manual = Manual::None;
        }
        self.recompute();
    }

    pub fn notify_child_exited(&mut self) {
        self.capturing = false;
        self.alt = false;
        self.bracketed = false;
        self.cursor_hidden = false;
        self.app_cursor = false;
        self.manual = Manual::None;
        self.apply(PaneState::DEFAULT);
    }

    /// Ctrl+\ — toggle the manual override (escape hatch / force-in).
    pub fn toggle_manual(&mut self) {
        self.manual = match self.state.input {
            InputMode::Raw => Manual::ForceCooked,
            InputMode::Cooked => Manual::ForceRaw,
        };
        // Manual override bypasses debounce.
        let t = self.target();
        self.apply(t);
    }

    pub fn observe(&mut self, sig: Signal) {
        use Signal::*;
        match sig {
            AltEnter => self.alt = true,
            AltExit => self.alt = false,
            BracketedEnter => self.bracketed = true,
            BracketedExit => self.bracketed = false,
            CursorHide => self.cursor_hidden = true,
            CursorShow => self.cursor_hidden = false,
            AppCursorEnter => self.app_cursor = true,
            AppCursorExit => self.app_cursor = false,
        }
        self.recompute();
    }
}

/// Mode signals extracted by the prescan (display/input hints only;
/// OSC-133 command boundaries are handled by the block parser).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    AltEnter,
    AltExit,
    BracketedEnter,
    BracketedExit,
    CursorHide,
    CursorShow,
    AppCursorEnter,
    AppCursorExit,
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Bug 2: an agent TUI never emits OSC-133 C/D, so a non-persistent
    // engine stays in BlockView and the reader never advances the grid
    // — `render_visible_snapshot` would be blank on (re-)attach. This
    // is the control: it documents the broken condition.
    #[test]
    fn non_persistent_without_capturing_is_block_view() {
        let mut sm = StateMachine::new();
        assert_eq!(sm.state().display, DisplayMode::BlockView);
        // Agent mode signals are ignored while not capturing.
        sm.observe(Signal::AltEnter);
        sm.observe(Signal::BracketedEnter);
        assert_eq!(
            sm.state().display,
            DisplayMode::BlockView,
            "without OSC-133 capturing the grid stays BlockView"
        );
    }

    // A persistent-program engine starts live (screen mode), so the
    // reader advances the grid from the first byte.
    #[test]
    fn persistent_starts_in_screen_mode() {
        let sm = StateMachine::new_persistent();
        assert_ne!(
            sm.state().display,
            DisplayMode::BlockView,
            "persistent engine must never be BlockView"
        );
        assert_eq!(sm.state().display, DisplayMode::InlineFull);
    }

    // The actual regression guard: a stray OSC-133 `D` (set_capturing
    // false) from the agent — or a tool it shells out to — must NOT
    // drop a persistent engine back to BlockView (which would freeze
    // the grid and blank the next re-attach).
    #[test]
    fn persistent_ignores_set_capturing_false() {
        let mut sm = StateMachine::new_persistent();
        sm.set_capturing(false);
        assert_ne!(sm.state().display, DisplayMode::BlockView);
        // And C/D churn never blanks it either.
        sm.set_capturing(true);
        sm.set_capturing(false);
        assert_ne!(sm.state().display, DisplayMode::BlockView);
    }

    // Alt-screen is still honoured for a persistent engine (vim inside
    // claude, a pager), and exiting alt-screen returns to InlineFull —
    // never BlockView.
    #[test]
    fn persistent_honours_alt_screen_without_block_view() {
        let mut sm = StateMachine::new_persistent();
        sm.observe(Signal::AltEnter);
        std::thread::sleep(DEBOUNCE + Duration::from_millis(20));
        sm.tick();
        assert_eq!(sm.state().display, DisplayMode::AltScreenFull);
        sm.observe(Signal::AltExit);
        // Entering a richer mode is debounced; settle it before asserting.
        assert_ne!(
            sm.state().display,
            DisplayMode::BlockView,
            "a persistent engine is never BlockView, even mid-transition"
        );
        std::thread::sleep(DEBOUNCE + Duration::from_millis(20));
        sm.tick();
        assert_eq!(
            sm.state().display,
            DisplayMode::InlineFull,
            "leaving alt-screen returns to InlineFull, never BlockView"
        );
    }
}

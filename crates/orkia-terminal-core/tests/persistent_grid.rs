// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// Regression coverage for the "black screen on re-attach" bug.
//
// An agent TUI (claude / codex / gemini) runs as the direct PTY child
// and never emits the OSC-133 C/D command markers a shell uses to
// bracket commands. The display-mode state machine forces `BlockView`
// while not "capturing", and the reader only advances the alacritty
// grid outside BlockView. So for an agent engine the grid was never
// advanced, and `render_visible_snapshot` — what the attach pump uses
// to rebuild the screen — came back blank. The first attach happened
// to work by replaying the buffered raw-output backlog; the second
// attach (backlog drained, agent idle) painted a blank grid: a black
// screen.
//
// `EngineConfig::persistent_program` marks agent engines so the grid
// is always live. These tests pin that: a persistent engine's grid
// captures plain (non-OSC-133) output, a non-persistent one does not.

use std::time::{Duration, Instant};

use orkia_terminal_core::{EngineConfig, TerminalEngine};

fn start(args: &[&str], persistent: bool) -> TerminalEngine {
    let config = EngineConfig {
        init_cols: 80,
        init_rows: 24,
        cmd: Some("sh".to_string()),
        args: args.iter().map(|s| s.to_string()).collect(),
        persistent_program: persistent,
        ..EngineConfig::default()
    };
    TerminalEngine::start(config).expect("engine start")
}

/// Poll `render_visible_snapshot` until it contains `needle` or the
/// deadline passes. Returns whether it appeared.
fn snapshot_shows(engine: &TerminalEngine, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let snap = String::from_utf8_lossy(&engine.render_visible_snapshot()).into_owned();
        if snap.contains(needle) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn persistent_engine_grid_reflects_agent_output() {
    // Plain output, no OSC-133 markers — exactly what a TUI agent draws.
    // The grid must go live so a re-attach can reconstruct the screen.
    let engine = start(&["-c", "printf 'PERSIST-MARKER'; sleep 5"], true);
    assert!(
        snapshot_shows(&engine, "PERSIST-MARKER", Duration::from_secs(4)),
        "persistent engine grid must capture agent output for re-attach"
    );
}

#[test]
fn non_persistent_engine_grid_stays_blank_without_osc133() {
    // Control: the same plain output with persistent_program = false
    // never reaches the grid (no OSC-133 C ever starts "capturing").
    // This documents the bug-2 failure mode and proves the test above
    // is actually load-bearing rather than passing vacuously.
    let engine = start(&["-c", "printf 'PLAIN-MARKER'; sleep 5"], false);
    assert!(
        !snapshot_shows(&engine, "PLAIN-MARKER", Duration::from_millis(800)),
        "without persistent_program the grid stays blank (the bug-2 condition)"
    );
}

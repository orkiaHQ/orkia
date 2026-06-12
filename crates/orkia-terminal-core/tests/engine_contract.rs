// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! End-to-end Reader → Extractor → Render contract, exercised through the
//! public API with a *fake PTY* (bytes fed directly into `BlockParser`, no
//! shell). Deterministic and CI-safe (no zsh, no gpui). Tests may `unwrap`.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use orkia_terminal_core::ansi::Snapshot;
use orkia_terminal_core::blocks::{
    BlockParser, BlocksState, SharedBlocks, Status, spawn_extractor,
};
use orkia_terminal_core::state::StateMachine;
use orkia_terminal_core::wake::wake_pair;
use parking_lot::Mutex;

/// Flatten a published snapshot to its visible text.
fn snap_text(s: &Snapshot) -> String {
    s.iter()
        .map(|line| line.iter().map(|sp| sp.text.as_str()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Wire the real pipeline (Reader-side parser + Extractor thread + the
/// wait-free Render read), with one running command block.
fn pipeline() -> (SharedBlocks, BlockParser) {
    let blocks: SharedBlocks = Arc::new(Mutex::new(BlocksState::default()));
    blocks.lock().push_command("test".to_string());
    let sm = Arc::new(Mutex::new(StateMachine::new()));
    let extract_dirty = Arc::new(AtomicBool::new(false));
    let (wake, _wake_rx) = wake_pair();
    spawn_extractor(Arc::clone(&blocks), Arc::clone(&extract_dirty), wake);
    let parser = BlockParser::new(Arc::clone(&blocks), sm, extract_dirty);
    (blocks, parser)
}

/// Poll the published snapshot (the render read path) until `pred` holds or
/// the deadline passes. Returns the last snapshot seen.
fn wait_for(blocks: &SharedBlocks, pred: impl Fn(&str) -> bool) -> Snapshot {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let snap = blocks.lock().blocks.last().unwrap().snapshot();
        if pred(&snap_text(&snap)) || Instant::now() >= deadline {
            return snap;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Bytes fed to the Reader appear in the rendered snapshot, and the OSC-133
/// exit code is captured.
#[test]
fn reader_extractor_render_reflects_bytes() {
    let (blocks, mut parser) = pipeline();
    parser.feed(b"\x1b]133;C\x07"); // command start (capturing on)
    parser.feed(b"the quick brown fox\n");
    parser.feed(b"jumps over the lazy dog\n");
    parser.feed(b"\x1b]133;D;0\x07"); // command end, exit 0

    let snap = wait_for(&blocks, |t| t.contains("lazy dog"));
    let text = snap_text(&snap);
    assert!(text.contains("the quick brown fox"), "got: {text:?}");
    assert!(text.contains("jumps over the lazy dog"), "got: {text:?}");
    assert_eq!(blocks.lock().blocks.last().unwrap().status, Status::Done(0));
}

/// An SGR escape split across two `feed` calls must still parse (the parser
/// keeps vte state across chunks — the boundary case the architecture doc
/// calls out).
#[test]
fn ansi_escape_split_across_chunks() {
    let (blocks, mut parser) = pipeline();
    parser.feed(b"\x1b]133;C\x07");
    parser.feed(b"\x1b[3"); // SGR sequence cut mid-stream
    parser.feed(b"1mCOLORED\x1b[0m text\n");
    parser.feed(b"\x1b]133;D;1\x07");

    let snap = wait_for(&blocks, |t| t.contains("COLORED"));
    assert!(snap_text(&snap).contains("COLORED text"));
    assert_eq!(blocks.lock().blocks.last().unwrap().status, Status::Done(1));
}

/// Edge cases must not panic and must still yield a sane snapshot:
/// zero-byte feeds, a pathologically long line, and raw control bytes.
#[test]
fn edge_cases_do_not_panic() {
    let (blocks, mut parser) = pipeline();
    parser.feed(b""); // zero-byte feed
    parser.feed(b"\x1b]133;C\x07");
    parser.feed(b""); // zero-byte feed while capturing
    let long = "x".repeat(100_000);
    parser.feed(long.as_bytes()); // pathological single line (width-clipped)
    parser.feed(b"\n");
    parser.feed(b"tail-marker\n");
    // Assorted raw control bytes incl. a dangling ESC — must not panic.
    parser.feed(&[0u8, 1, 2, 3, 7, 8, 27, 127]);
    parser.feed(b"\x1b]133;D;0\x07");

    // The engine survives (no panic) and the pre-control marker is rendered;
    // the grid is width-bounded so the 100k line is clipped, not crashing.
    let snap = wait_for(&blocks, |t| t.contains("tail-marker"));
    assert!(snap_text(&snap).contains("tail-marker"));
    assert_eq!(blocks.lock().blocks.last().unwrap().status, Status::Done(0));
}

/// A block with no output still publishes an (empty) snapshot — the render
/// path never sees an uninitialised block.
#[test]
fn empty_block_has_snapshot() {
    let blocks: SharedBlocks = Arc::new(Mutex::new(BlocksState::default()));
    blocks.lock().push_command("noop".to_string());
    let snap = blocks.lock().blocks.last().unwrap().snapshot();
    // pack(Vec::new()) → an Arc to an empty Vec; cloning it is wait-free.
    assert!(snap.is_empty());
}

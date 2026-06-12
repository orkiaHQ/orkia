// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! PR1 smoke test for the reader-thread `ScreenSnapshot` publish path.
//!
//! Strategy: build a `TerminalEngine` via `adopt_master` over a real
//! `open_pair` PTY (no child process — we drive the slave fd directly
//! from the test thread). Write some bytes to the slave; assert that
//! the engine reader thread publishes a fresh `ScreenSnapshot` (the
//! generation counter increments) and that the published ANSI body
//! contains the bytes we wrote.
//!
//! No agent binary involved.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::io::FromRawFd;
use std::time::{Duration, Instant};

use orkia_pty::AdoptedPty;
use orkia_terminal_core::{AdoptMaster, EngineConfig, TerminalEngine};

/// Spin up an `adopt_master` engine over a real PTY pair. Returns the
/// engine + the slave fd as a writable `File` so the test thread can
/// push bytes that the engine reader thread will read from the master
/// side.
fn engine_over_pair() -> (TerminalEngine, std::fs::File) {
    let pair: AdoptedPty = orkia_pty::open_pair(80, 24).expect("open_pair");
    let AdoptedPty {
        reader,
        writer,
        master_fd,
        slave,
        dims,
        screen,
    } = pair;
    // Re-open the slave fd as a regular File so the test can `write_all`.
    // We dup the fd so the original `OwnedFd` in `slave` keeps the slave
    // end alive for the duration of the test (closing it would EOF the
    // engine's reader thread immediately).
    let slave_raw = slave.as_raw_fd();
    // SAFETY: `dup` returns a fresh fd referring to the same open file
    // description; we own it for the lifetime of the returned File.
    let dup = unsafe { libc::dup(slave_raw) };
    assert!(
        dup >= 0,
        "dup(slave) failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: `dup` returned a valid fd; we have exclusive ownership.
    let slave_writer = unsafe { std::fs::File::from_raw_fd(dup) };
    // Leak `slave` (move it into a Box) so the slave end stays open. If
    // we let `slave` drop here, the master reader would see EOF and the
    // engine reader thread would exit before we can drive it.
    Box::leak(Box::new(slave));

    let engine = TerminalEngine::adopt_master(AdoptMaster {
        reader,
        writer,
        master_fd,
        dims,
        screen,
        buf_bytes: 4096,
        on_osc133: None,
        on_apc: None,
    })
    .expect("adopt_master");
    let _ = EngineConfig::default(); // Sanity-touch the config import so it stays used.
    (engine, slave_writer)
}

/// Poll the screen view until `pred(snapshot)` is true or the deadline
/// expires. Returns the last observed snapshot.
fn wait_for_generation(
    engine: &TerminalEngine,
    deadline: Instant,
    pred: impl Fn(u64, &[u8]) -> bool,
) -> (u64, Vec<u8>) {
    let view = engine.screen_view();
    loop {
        let snap = view.load();
        let g = snap.generation;
        let bytes: Vec<u8> = snap.ansi.to_vec();
        if pred(g, &bytes) {
            return (g, bytes);
        }
        if Instant::now() >= deadline {
            return (g, bytes);
        }
        // Don't hold the guard across the sleep.
        drop(snap);
        std::thread::sleep(Duration::from_millis(8));
    }
}

#[test]
fn initial_snapshot_is_empty_and_generation_zero() {
    let (engine, _slave_writer) = engine_over_pair();
    let view = engine.screen_view();
    let snap = view.load();
    assert_eq!(
        snap.generation, 0,
        "fresh engine must publish gen 0 sentinel"
    );
    assert!(
        snap.ansi.is_empty(),
        "no bytes written yet — ansi body must be empty"
    );
    assert_eq!(snap.cols, 80);
    assert_eq!(snap.rows, 24);
}

/// Drive the engine into a screen mode (`InlineFull` / `AltScreenFull`).
/// The engine defaults to `BlockView`, in which the reader thread
/// intentionally skips the screen grid and the snapshot publish; the
/// only way to exercise the publish path is to flip the state machine
/// into a screen mode first.
///
/// We use OSC 133 ;C (command-start → `capturing = true`) followed by
/// the alt-screen enter escape `\x1b[?1049h` (`alt = true`). Both are
/// observed by the prescan + state machine in the reader thread.
///
/// The state machine debounces "enter a richer mode" by 100 ms (see
/// `state::DEBOUNCE`); we sleep just past that window before sending
/// the alt-screen escape so the transition applies on its first
/// observation rather than landing in `pending`.
fn enter_screen_mode(w: &mut std::fs::File) {
    // OSC 133 ;C — command start (`capturing = true`).
    w.write_all(b"\x1b]133;C\x07").expect("osc133 C");
    w.flush().expect("flush osc133");
    // Wait out the state-machine debounce so the alt-enter observe
    // applies immediately (no need to call `tick` from outside the
    // engine).
    std::thread::sleep(Duration::from_millis(120));
    // Enter alt-screen — flips StateMachine to `display = AltScreenFull`.
    w.write_all(b"\x1b[?1049h").expect("alt-screen enter");
    w.flush().expect("flush alt-enter");
    // Give the reader thread a moment to process the escape before
    // the test issues its content writes.
    std::thread::sleep(Duration::from_millis(30));
}

#[test]
fn bytes_written_appear_in_published_snapshot() {
    let (engine, mut slave_writer) = engine_over_pair();
    enter_screen_mode(&mut slave_writer);

    // Push a recognisable marker through the slave so the engine reader
    // thread parses it into the alacritty grid and publishes a snapshot.
    slave_writer
        .write_all(b"hello-snapshot-marker")
        .expect("write slave");
    slave_writer.flush().expect("flush slave");

    // Wait up to 2 s for the publish budget to fire. With min_publish =
    // 16 ms this is a very generous window — the test is racing the
    // reader thread, not the wall clock.
    let deadline = Instant::now() + Duration::from_secs(2);
    let needle: &[u8] = b"hello-snapshot-marker";
    let (gn, bytes) = wait_for_generation(&engine, deadline, |g, body| {
        g > 0 && body.windows(needle.len()).any(|w| w == needle)
    });

    assert!(
        gn > 0,
        "publisher must have advanced past sentinel (got gen={gn})"
    );
    let body = String::from_utf8_lossy(&bytes);
    assert!(
        body.contains("hello-snapshot-marker"),
        "rendered body must echo what was written, got: {body:?}"
    );
}

#[test]
fn generation_is_monotonic_across_writes() {
    let (engine, mut slave_writer) = engine_over_pair();
    enter_screen_mode(&mut slave_writer);

    // First write — should advance generation past 0.
    slave_writer.write_all(b"first ").expect("write 1");
    slave_writer.flush().expect("flush 1");

    let deadline_a = Instant::now() + Duration::from_secs(2);
    let (gen_a, _) = wait_for_generation(&engine, deadline_a, |g, body| g > 0 && !body.is_empty());
    assert!(gen_a > 0, "first write must publish");

    // Second write — wait long enough that the 16 ms budget has elapsed
    // before issuing the bytes, otherwise the reader may coalesce both
    // into a single publish (which is correct behaviour but not what
    // this test exercises).
    std::thread::sleep(Duration::from_millis(40));
    slave_writer.write_all(b"second").expect("write 2");
    slave_writer.flush().expect("flush 2");

    let deadline_b = Instant::now() + Duration::from_secs(2);
    let (gen_b, body_b) = wait_for_generation(&engine, deadline_b, |g, body| {
        g > gen_a
            && String::from_utf8_lossy(body).contains("first")
            && String::from_utf8_lossy(body).contains("second")
    });

    assert!(
        gen_b > gen_a,
        "second write must produce a fresh generation (got {gen_a} -> {gen_b})"
    );
    let body = String::from_utf8_lossy(&body_b);
    assert!(body.contains("first"), "first write lost: {body:?}");
    assert!(body.contains("second"), "second write lost: {body:?}");
}

#[test]
fn screen_view_clone_observes_same_publish_slot() {
    // Two `ScreenView` clones must observe the same `Arc<ScreenSnapshot>`
    // pointer at any instant. This is the property that lets PR2+
    // hand the view to multiple consumers without coordination.
    let (engine, mut slave_writer) = engine_over_pair();
    enter_screen_mode(&mut slave_writer);

    let v1 = engine.screen_view();
    let v2 = engine.screen_view();

    slave_writer.write_all(b"shared-publish").expect("write");
    slave_writer.flush().expect("flush");

    let deadline = Instant::now() + Duration::from_secs(2);
    let _ = wait_for_generation(&engine, deadline, |g, _| g > 0);

    let a = v1.snapshot();
    let b = v2.snapshot();
    assert!(
        std::sync::Arc::ptr_eq(&a, &b),
        "two views over the same engine must observe the same Arc — \
         shared publish slot is the invariant under test"
    );
}

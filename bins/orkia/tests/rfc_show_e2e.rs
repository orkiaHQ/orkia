// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! E2E (T2.54a / Finding #2): `rfc show <slug>` is slug-addressed and
//! resolves project-less, mirroring `rfc list`.
//!
//! Drives the shipped `orkia --no-tui` binary headlessly — the exact shape
//! of the finding's repro (`printf '…' | "$ORK" --no-tui`) — and asserts on
//! the rendered stdout. `EDITOR=true` keeps `rfc create`'s editor a no-op.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use tempfile::TempDir;

fn orkia_bin() -> &'static str {
    env!("CARGO_BIN_EXE_orkia")
}

/// Serializes the `Command::spawn` window across the test threads. On macOS
/// `std` lacks atomic `pipe2(O_CLOEXEC)`; it sets `CLOEXEC` with a separate
/// `ioctl` after `pipe()`. When these slug-addressed cases run in parallel,
/// one thread's `fork` can land inside that window and inherit another
/// thread's stdin pipe end — pinning the victim shell's `read_line` open so it
/// never sees EOF (a 6-hour hang). Holding this lock only across `spawn`
/// closes the window while keeping the tests otherwise parallel.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// Run a newline-separated REPL script through `orkia --no-tui` in a fresh
/// `$HOME`, returning combined stdout+stderr.
fn run_script(home: &TempDir, script: &str) -> String {
    let spawned = {
        let _guard = SPAWN_LOCK.lock().expect("spawn lock");
        Command::new(orkia_bin())
            .arg("--no-tui")
            .env("HOME", home.path())
            .env("EDITOR", "true")
            .env("VISUAL", "true")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    };
    let mut child = spawned.expect("spawn orkia --no-tui");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(script.as_bytes())
        .expect("write script");
    let out = child.wait_with_output().expect("wait orkia");
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    combined
}

#[test]
fn rfc_show_slug_addressed_project_less() {
    let home = TempDir::new().expect("tmp home");

    // Seed two projects sharing a slug, plus one project-unique slug.
    let out = run_script(
        &home,
        "project create demo \"d\"\n\
         project create pay \"p\"\n\
         rfc create \"auth refresh\" --project demo\n\
         rfc create \"auth refresh\" --project pay\n\
         rfc create \"solo idea\" --project pay\n\
         rfc show auth-refresh\n\
         rfc show auth-refresh --project demo\n\
         rfc show solo-idea\n\
         rfc show ghost-slug\n",
    );

    // Collision project-less → refuses, names both projects, points at --project.
    assert!(
        out.contains("rfc 'auth-refresh' exists in demo, pay; disambiguate with --project"),
        "collision must list candidates; got:\n{out}"
    );
    // --project disambiguates → renders the RFC body.
    assert!(
        out.contains("# auth refresh"),
        "--project must render the RFC; got:\n{out}"
    );
    // Unique slug resolves project-less.
    assert!(
        out.contains("# solo idea"),
        "unique slug must resolve project-less; got:\n{out}"
    );
    // Unknown slug fails closed with a clear message.
    assert!(
        out.contains("rfc 'ghost-slug' not found in any project"),
        "unknown slug must 404 clearly; got:\n{out}"
    );
    // The legacy project-less error must be gone for resolvable slugs.
    assert!(
        !out.contains("no project specified and no default available"),
        "slug-addressed show must not emit the legacy project-context error; got:\n{out}"
    );
}

#[test]
fn rfc_state_and_lock_status_slug_addressed() {
    let home = TempDir::new().expect("tmp home");

    let out = run_script(
        &home,
        "project create demo \"d\"\n\
         project create pay \"p\"\n\
         rfc create \"auth refresh\" --project demo\n\
         rfc create \"auth refresh\" --project pay\n\
         rfc create \"solo idea\" --project pay\n\
         rfc state auth-refresh\n\
         rfc state solo-idea\n\
         rfc lock-status auth-refresh\n\
         rfc lock-status solo-idea\n\
         rfc state ghost-slug\n",
    );

    // Collision is fail-closed for both verbs.
    assert!(
        out.contains("rfc 'auth-refresh' exists in demo, pay; disambiguate with --project"),
        "collision must fail closed for state/lock-status; got:\n{out}"
    );
    // Unique slug resolves project-less: state renders, lock-status renders.
    assert!(
        out.contains("rfc:solo-idea state="),
        "rfc state must resolve a unique slug project-less; got:\n{out}"
    );
    assert!(
        out.contains("rfc:solo-idea unlocked"),
        "rfc lock-status must resolve a unique slug project-less; got:\n{out}"
    );
    // Unknown slug stays fail-closed.
    assert!(
        out.contains("rfc 'ghost-slug' not found in any project"),
        "unknown slug must 404 clearly; got:\n{out}"
    );
}

#[test]
fn rfc_cd_scope_outranks_collision() {
    let home = TempDir::new().expect("tmp home");

    let out = run_script(
        &home,
        "project create demo \"d\"\n\
         project create pay \"p\"\n\
         rfc create \"auth refresh\" --project demo\n\
         rfc create \"auth refresh\" --project pay\n\
         rfc cd auth-refresh --project demo\n\
         rfc show auth-refresh\n\
         rfc state\n\
         rfc exit\n\
         rfc show auth-refresh\n",
    );

    // In scope, the colliding slug resolves to demo: the RFC renders and a
    // bare `rfc state` defaults slug+project from the scope.
    assert!(
        out.contains("# auth refresh"),
        "in-scope show must render the scoped RFC; got:\n{out}"
    );
    assert!(
        out.contains("rfc:auth-refresh state="),
        "bare `rfc state` must default slug+project from the scope; got:\n{out}"
    );
    // After `rfc exit`, the project-less collision behaviour returns.
    assert!(
        out.contains("rfc 'auth-refresh' exists in demo, pay; disambiguate with --project"),
        "after `rfc exit` the collision fallback must apply again; got:\n{out}"
    );
}

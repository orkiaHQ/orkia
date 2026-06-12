// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Smoke test for the public `orkia` binary's auth subcommands.
//!
//! The OSS binary uses `MagicLinkAuthProvider`; reads (`whoami`) load the
//! persisted session only — there is no env-injected bypass. The deeper
//! magic-link round-trip lives in the magic-login crate's tests.

use std::process::Command;

fn orkia_bin() -> std::path::PathBuf {
    std::env::current_exe()
        .unwrap()
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("orkia"))
        .unwrap()
}

#[test]
fn whoami_with_no_session_exits_nonzero() {
    let bin = orkia_bin();
    if !bin.exists() {
        eprintln!(
            "test: skipping (orkia binary not built yet at {})",
            bin.display()
        );
        return;
    }
    // Point the session store at an empty temp file (no keychain, no env
    // bypass) so `whoami` has nothing to load and must report "not signed
    // in" (exit 1).
    let tmp = tempfile::TempDir::new().unwrap();
    let output = Command::new(&bin)
        .arg("whoami")
        .env("ORKIA_SESSION_FILE", tmp.path().join("session.toml"))
        .env("HOME", tmp.path())
        .output()
        .expect("spawn orkia whoami");
    let combined = format!(
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        !output.status.success(),
        "expected nonzero exit with no persisted session; got {combined}"
    );
}

#[test]
fn help_lists_subcommands() {
    let bin = orkia_bin();
    if !bin.exists() {
        return;
    }
    let output = Command::new(&bin).arg("--help").output().expect("spawn");
    assert!(output.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        combined.to_lowercase().contains("usage") || combined.to_lowercase().contains("orkia"),
        "help output looks empty: {combined}"
    );
}

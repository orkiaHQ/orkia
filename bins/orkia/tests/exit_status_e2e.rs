// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! The REPL pipeline path (`orkia <builtin>`) propagates the outcome's
//! code; the bare shell path keeps brush's POSIX codes (127 untouched).
//! In a separate file from `cli_test.rs` (at its size cap).

use std::process::Command;

fn orkia_bin() -> &'static str {
    env!("CARGO_BIN_EXE_orkia")
}

fn run_dash_c(home: &std::path::Path, cmd: &str) -> std::process::Output {
    Command::new(orkia_bin())
        .env("HOME", home)
        .args(["-c", cmd])
        .output()
        .expect("spawn orkia")
}

#[test]
fn builtin_usage_error_exits_2() {
    let home = tempfile::tempdir().expect("temp home");
    let out = run_dash_c(home.path(), "orkia ps --bogus");
    assert_eq!(
        out.status.code(),
        Some(2),
        "unknown flag is a usage error; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn builtin_success_exits_0() {
    let home = tempfile::tempdir().expect("temp home");
    let out = run_dash_c(home.path(), "orkia ps");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn builtin_runtime_error_exits_1() {
    let home = tempfile::tempdir().expect("temp home");
    let out = run_dash_c(home.path(), "orkia stop zz-no-such-job");
    assert_eq!(
        out.status.code(),
        Some(1),
        "well-formed invocation, runtime miss; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The bare shell path is brush's: command-not-found keeps POSIX 127 —
/// reserved codes only ever come from real process execution.
#[test]
fn shell_not_found_exits_127() {
    let home = tempfile::tempdir().expect("temp home");
    let out = run_dash_c(home.path(), "zz-no-such-binary-orkia-spec7");
    assert_eq!(
        out.status.code(),
        Some(127),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

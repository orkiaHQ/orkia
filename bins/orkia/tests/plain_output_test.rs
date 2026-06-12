// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! contain no ANSI escapes. `Command::output()` pipes stdout, so the
//! protects. In a separate file from `cli_test.rs` (at its size cap).

use std::process::Command;

fn orkia_bin() -> &'static str {
    env!("CARGO_BIN_EXE_orkia")
}

/// Piped `-c "orkia ls"` renders a typed table through the REPL pipeline
/// and the `StdoutRenderer`: stdout must carry the rows escape-free.
#[test]
fn dash_c_piped_table_stdout_has_no_escapes() {
    let home = tempfile::tempdir().expect("temp home");
    let workdir = tempfile::tempdir().expect("temp workdir");
    std::fs::write(workdir.path().join("greppable.txt"), "x").expect("seed file");

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .current_dir(workdir.path())
        .args(["-c", "orkia ls"])
        .output()
        .expect("spawn orkia");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("greppable.txt"),
        "expected a table row on stdout: {stdout:?}"
    );
    assert!(
        !out.stdout.contains(&0x1b),
        "non-TTY stdout must contain no ESC byte: {stdout:?}"
    );
}

/// Same invariant for the error path: `error:` lines degrade to plain text.
#[test]
fn dash_c_piped_error_output_has_no_escapes() {
    let home = tempfile::tempdir().expect("temp home");
    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["-c", "orkia ls /nonexistent-orkia-spec6-path"])
        .output()
        .expect("spawn orkia");
    assert!(
        !out.stdout.contains(&0x1b),
        "stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !out.stderr.contains(&0x1b),
        "block-rendered stderr must also degrade plain: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

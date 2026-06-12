// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! End-to-end tests for `orkia setup`.
//!
//! We run the real binary against a tempdir HOME so the wizard can't
//! see the developer's actual `~/.orkia`. The wizard's interactive
//! path is exercised via the `--minimal` flag (which does not read
//! stdin); the registry sync is skipped via `--offline` so tests are
//! hermetic and don't require network or `git` on PATH.

use std::process::Command;
use tempfile::TempDir;

fn orkia_bin() -> &'static str {
    env!("CARGO_BIN_EXE_orkia")
}

fn run_setup(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(orkia_bin())
        .arg("setup")
        .args(args)
        .env("HOME", home)
        .env("ORKIA_NONINTERACTIVE", "1")
        .output()
        .expect("spawn orkia setup")
}

#[test]
fn minimal_creates_base_dirs_without_prompting() {
    let home = TempDir::new().unwrap();
    let out = run_setup(home.path(), &["--minimal"]);
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let dot_orkia = home.path().join(".orkia");
    for sub in ["agents", "projects", "run", "registry"] {
        assert!(dot_orkia.join(sub).is_dir(), "missing {sub}");
    }
    for f in ["seal.jsonl", "journal.jsonl", "history", "config.toml"] {
        assert!(dot_orkia.join(f).is_file(), "missing {f}");
    }
}

#[test]
fn minimal_is_idempotent() {
    let home = TempDir::new().unwrap();
    assert!(run_setup(home.path(), &["--minimal"]).status.success());
    // Mutate config so we can prove the second run doesn't clobber it.
    let cfg = home.path().join(".orkia").join("config.toml");
    std::fs::write(&cfg, "load_bashrc = false\n").unwrap();
    assert!(run_setup(home.path(), &["--minimal"]).status.success());
    assert_eq!(
        std::fs::read_to_string(&cfg).unwrap(),
        "load_bashrc = false\n"
    );
}

#[test]
fn force_wipes_existing_orkia_dir() {
    let home = TempDir::new().unwrap();
    assert!(run_setup(home.path(), &["--minimal"]).status.success());
    let marker = home.path().join(".orkia").join("agents").join("MARKER");
    std::fs::write(&marker, "x").unwrap();
    assert!(marker.exists());

    assert!(
        run_setup(home.path(), &["--minimal", "--force"])
            .status
            .success()
    );
    assert!(
        !marker.exists(),
        "--force should have removed the agents/ contents",
    );
    // …but the freshly-created base dir still exists.
    assert!(home.path().join(".orkia").join("agents").is_dir());
}

#[test]
fn help_flag_exits_zero_and_prints_usage() {
    let home = TempDir::new().unwrap();
    let out = run_setup(home.path(), &["--help"]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("orkia setup"), "stderr was: {stderr}");
    assert!(stderr.contains("--minimal"));
    assert!(stderr.contains("--offline"));
}

#[test]
fn unknown_flag_exits_nonzero() {
    let home = TempDir::new().unwrap();
    let out = run_setup(home.path(), &["--bogus"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown setup flag"));
}

#[test]
fn offline_minimal_does_not_clone_registry() {
    let home = TempDir::new().unwrap();
    let out = run_setup(home.path(), &["--minimal", "--offline"]);
    assert!(out.status.success());
    // --minimal skips the wizard entirely, so the registry dir should
    // be the empty placeholder created by create_base_dirs (no .git/).
    let reg = home
        .path()
        .join(".orkia")
        .join("registry")
        .join("archetypes");
    assert!(
        !reg.join(".git").exists(),
        "registry should not be cloned in minimal mode"
    );
}

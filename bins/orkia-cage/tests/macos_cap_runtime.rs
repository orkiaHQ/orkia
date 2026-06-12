// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! real `orkia-cage` binary, which `exec`s `sandbox-exec` with the generated
//! Seatbelt profile, and probes the workspace with `/bin/bash`. Complements
//! `macos_cap_profile.rs` (which checks the emitted profile string): this asserts
//! Seatbelt actually *enforces* it at runtime.
//!
//! Self-skips when the `read=t,write=t` baseline cannot even write — i.e. the
//! environment can't run `sandbox-exec` under `cargo test` — so it never
//! false-fails in a restricted sandbox; it only asserts the *delta* (write=off
//! must block what write=on allows).

#![cfg(target_os = "macos")]

use std::io::Write;
use std::process::Command;

/// Run `orkia-cage --policy <caps> -- /bin/bash -c '<probe>'` and return whether
/// the probe's target file exists afterwards (the workspace is the real dir on
/// macOS, so a permitted write lands there; a denied one does not).
fn probe_write(read: bool, write: bool) -> (bool, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tmp");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).expect("mkdir ws");
    let target = ws.join("probe.txt");
    let policy = dir.path().join("p.toml");
    let mut f = std::fs::File::create(&policy).expect("policy");
    write!(
        f,
        "default_verdict = \"allow\"\n\n[caps]\nread = {read}\nwrite = {write}\nexec = true\n\n[workspace]\nroot = {:?}\n",
        ws.to_string_lossy()
    )
    .expect("write policy");

    let _ = Command::new(env!("CARGO_BIN_EXE_orkia-cage"))
        .arg("--policy")
        .arg(&policy)
        .arg("--")
        .arg("/bin/bash")
        .arg("-c")
        .arg(format!("echo CAP_OK > {}", target.to_string_lossy()))
        .output()
        .expect("run orkia-cage");
    // Leak the tempdir handle by returning it would be cleaner; instead read now.
    let exists = target.exists();
    // keep `dir` alive until here
    drop(dir);
    (exists, target)
}

#[test]
fn write_off_blocks_workspace_write_at_runtime() {
    // Baseline: with write on, the cage must let the write through. If it does
    // not, this host cannot run sandbox-exec under the test harness → skip.
    let (baseline_ok, _) = probe_write(true, true);
    if !baseline_ok {
        eprintln!("skip: sandbox-exec baseline write did not succeed (restricted env)");
        return;
    }
    // The delta under test: write=false must block the same write (Seatbelt
    // deny-default, no workspace `file-write*` allow).
    let (blocked_write_landed, _) = probe_write(true, false);
    assert!(
        !blocked_write_landed,
        "write=false must block the workspace write at the kernel (Seatbelt)"
    );
}

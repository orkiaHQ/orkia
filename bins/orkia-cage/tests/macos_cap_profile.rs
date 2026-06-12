// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! through the **real cage binary**: a policy file → CLI parse → `load_policy`
//! (which prints the SBPL and exits without `exec`'ing `sandbox-exec`).
//!
//! The unit tests in `sbpl.rs` cover `build_profile` in isolation; this proves
//! the whole binary pipeline honours `caps.read`/`caps.write`. Kernel-level
//! Seatbelt behaviour is Apple's and is exercised manually (see
//! `qa/cap-classes.md`); here we assert the profile the cage actually emits.

#![cfg(target_os = "macos")]

use std::io::Write;
use std::process::Command;

fn dumped_profile(read: bool, write: bool) -> String {
    let dir = tempfile::tempdir().expect("tmp");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).expect("mkdir ws");
    let policy = dir.path().join("p.toml");
    let mut f = std::fs::File::create(&policy).expect("policy");
    write!(
        f,
        "default_verdict = \"allow\"\n\n[caps]\nread = {read}\nwrite = {write}\nexec = true\n\n[workspace]\nroot = {:?}\n",
        ws.to_string_lossy()
    )
    .expect("write policy");

    let out = Command::new(env!("CARGO_BIN_EXE_orkia-cage"))
        .env("ORKIA_CAGE_DEBUG_PROFILE", "1")
        .arg("--policy")
        .arg(&policy)
        .arg("--")
        .arg("/bin/echo")
        .arg("hi")
        .output()
        .expect("run orkia-cage");
    assert!(
        out.status.success(),
        "cage --debug-profile exited nonzero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let profile = String::from_utf8_lossy(&out.stdout).into_owned();
    // The dumped profile references the canonicalized workspace path.
    let canon = std::fs::canonicalize(&ws).unwrap_or(ws);
    format!("{profile}\n__WS__={}", canon.to_string_lossy())
}

fn ws_path(dump: &str) -> String {
    dump.rsplit("__WS__=").next().unwrap().trim().to_string()
}

#[test]
fn write_off_omits_workspace_write_allow() {
    let dump = dumped_profile(true, false);
    let ws = ws_path(&dump);
    assert!(
        dump.contains(&format!("(allow file-read* (subpath \"{ws}\"))")),
        "read=true must keep the workspace read-allow"
    );
    assert!(
        !dump.contains(&format!("(allow file-write* (subpath \"{ws}\"))")),
        "write=false must omit the workspace write-allow (read-only)"
    );
}

#[test]
fn read_off_omits_workspace_entirely() {
    let dump = dumped_profile(false, false);
    let ws = ws_path(&dump);
    assert!(
        !dump.contains(&format!("(allow file-read* (subpath \"{ws}\"))")),
        "read=false must omit the workspace read-allow (ENOENT)"
    );
    assert!(
        !dump.contains(&format!("(allow file-write* (subpath \"{ws}\"))")),
        "read=false implies no workspace write-allow"
    );
}

#[test]
fn read_write_on_grants_both() {
    let dump = dumped_profile(true, true);
    let ws = ws_path(&dump);
    assert!(dump.contains(&format!("(allow file-read* (subpath \"{ws}\"))")));
    assert!(dump.contains(&format!("(allow file-write* (subpath \"{ws}\"))")));
}

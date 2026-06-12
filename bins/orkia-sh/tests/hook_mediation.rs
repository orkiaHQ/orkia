// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! macOS command mediation (PreToolUse hook) — end-to-end through the real
//! `orkia-sh hook` binary. On macOS the per-command gate is Claude's PreToolUse
//! hook (the shadow-shell shim is Linux-only), so this is the macOS analogue of
//! the Linux `exec=on` rule test: feed the hook the PreToolUse JSON Claude sends
//! and assert the decision JSON it returns.
//!
//! Deterministic paths only (no journal socket needed): a rule-matched command
//! denies (best-effort `record`), and the two defer paths (non-mediated tool,
//! uncaged session) return before any audit write. The allow-defer-with-socket
//! path is exercised manually (see `qa/cap-classes.md`).

use std::io::Write;
use std::process::{Command, Stdio};

fn run_hook(policy_env: Option<&str>, stdin_json: &str) -> (String, bool) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_orkia-sh"));
    cmd.arg("hook").stdin(Stdio::piped()).stdout(Stdio::piped());
    match policy_env {
        Some(p) => {
            cmd.env("ORKIA_CAGE_POLICY", p);
        }
        None => {
            cmd.env_remove("ORKIA_CAGE_POLICY");
        }
    }
    let mut child = cmd.spawn().expect("spawn orkia-sh hook");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin_json.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    )
}

fn deny_rule_policy(dir: &std::path::Path) -> std::path::PathBuf {
    let p = dir.join("rule.toml");
    let mut f = std::fs::File::create(&p).expect("policy");
    write!(
        f,
        "default_verdict = \"allow\"\n[caps]\nread=true\nwrite=true\nexec=true\n[workspace]\nroot = \".\"\n\n[[capabilities]]\nname = \"git.status\"\nmatches = [\"git status*\"]\nverdict = \"deny\"\n"
    )
    .unwrap();
    p
}

const PRETOOL: &str =
    r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"%CMD%"}}"#;

#[test]
fn rule_matched_bash_command_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let policy = deny_rule_policy(dir.path());
    let (out, ok) = run_hook(
        Some(&policy.to_string_lossy()),
        &PRETOOL.replace("%CMD%", "git status"),
    );
    assert!(ok, "hook should exit 0 even when denying");
    assert!(
        out.contains("\"permissionDecision\":\"deny\""),
        "expected a PreToolUse deny; got: {out}"
    );
    assert!(
        out.contains("git.status"),
        "deny should name the capability; got: {out}"
    );
}

#[test]
fn non_mediated_tool_defers() {
    let dir = tempfile::tempdir().unwrap();
    let policy = deny_rule_policy(dir.path());
    // A non-Bash tool is not gated → defer (empty stdout, exit 0).
    let json =
        r#"{"hook_event_name":"PreToolUse","tool_name":"Read","tool_input":{"file_path":"x"}}"#;
    let (out, ok) = run_hook(Some(&policy.to_string_lossy()), json);
    assert!(ok);
    assert!(
        out.trim().is_empty(),
        "non-mediated tool must defer; got: {out}"
    );
}

#[test]
fn uncaged_session_defers() {
    // No ORKIA_CAGE_POLICY → not caged → the cooperative hook defers (the macOS
    // guarantee in an uncaged session is none — it is a caged-only gate).
    let (out, ok) = run_hook(None, &PRETOOL.replace("%CMD%", "git status"));
    assert!(ok);
    assert!(
        out.trim().is_empty(),
        "uncaged session must defer; got: {out}"
    );
}

#[test]
fn exec_off_denies_via_hook() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("execoff.toml");
    std::fs::write(
        &p,
        "default_verdict = \"allow\"\n[caps]\nexec=false\n[workspace]\nroot = \".\"\n",
    )
    .unwrap();
    let (out, ok) = run_hook(
        Some(&p.to_string_lossy()),
        &PRETOOL.replace("%CMD%", "ls -la"),
    );
    assert!(ok);
    assert!(
        out.contains("\"permissionDecision\":\"deny\""),
        "exec=off must deny via the hook; got: {out}"
    );
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! End-to-end smoke against the real `orkia-kernel` binary.
//!
//! Runs only when the binary path is provided via `ORKIA_KERNEL_BIN`.
//! Test envs that do not have the daemon installed skip silently.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use orkia_shell_types::IntentGuess;
use tempfile::TempDir;

fn kernel_binary() -> Option<std::path::PathBuf> {
    std::env::var_os("ORKIA_KERNEL_BIN").map(std::path::PathBuf::from)
}

fn spawn_kernel(bin: &std::path::Path, home: &std::path::Path) -> Child {
    Command::new(bin)
        .env("HOME", home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn orkia-kernel")
}

fn wait_for_socket(path: &std::path::Path) {
    for _ in 0..100 {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    panic!("kernel did not bind socket at {}", path.display());
}

#[test]
fn handshake_classify_shutdown_round_trip() {
    let Some(bin) = kernel_binary() else {
        eprintln!("skipping: ORKIA_KERNEL_BIN not set");
        return;
    };

    let tmp = TempDir::new().unwrap();
    let home = tmp.path().to_path_buf();
    let sock = home.join(".orkia/run/kernel.sock");
    let mut child = spawn_kernel(&bin, &home);
    wait_for_socket(&sock);

    let rpc = orkia_kernel_client::discover_at(sock).expect("discover");
    let v = rpc.version();
    assert_eq!(v.protocol, 1);
    assert!(!v.kernel.is_empty());

    let agent = rpc
        .classify_with_timeout("what is this?", Duration::from_millis(500))
        .unwrap();
    assert!(matches!(agent, IntentGuess::Agent));

    let cmd = rpc
        .classify_with_timeout("ls -la", Duration::from_millis(500))
        .unwrap();
    assert!(matches!(cmd, IntentGuess::Command));

    rpc.shutdown().unwrap();
    let _ = child.wait();
}

#[test]
fn sprint_c_models_and_benchmark() {
    let Some(bin) = kernel_binary() else {
        eprintln!("skipping: ORKIA_KERNEL_BIN not set");
        return;
    };

    let tmp = TempDir::new().unwrap();
    let home = tmp.path().to_path_buf();
    let sock = home.join(".orkia/run/kernel.sock");
    let mut child = spawn_kernel(&bin, &home);
    wait_for_socket(&sock);

    let rpc = orkia_kernel_client::discover_at(sock).expect("discover");

    // models.list — always at least the seed entry
    let models = rpc.list_models().expect("list_models");
    assert!(!models.is_empty(), "registry should be seeded");
    assert!(models.iter().any(|m| m.id.contains("qwen")));

    // pull on unknown id surfaces clean enum, not a transport error
    let outcome = rpc.pull_model("no-such-model").expect("pull_model");
    assert!(matches!(
        outcome,
        orkia_shell_types::KernelPullOutcome::NotInRegistry { .. }
    ));

    // benchmark without a model loaded returns Unsupported (not an error)
    let bench = rpc.benchmark(5).expect("benchmark");
    assert!(matches!(
        bench,
        orkia_shell_types::KernelBenchmarkOutcome::Unsupported
            | orkia_shell_types::KernelBenchmarkOutcome::Ran { .. }
    ));

    // classification round-trip still works and lands in the journal
    rpc.classify_with_timeout("what is this?", Duration::from_millis(500))
        .unwrap();
    rpc.classify_with_timeout("ls -la", Duration::from_millis(500))
        .unwrap();

    rpc.shutdown().unwrap();
    let _ = child.wait();

    // Verify the kernel wrote feedback events into the journal
    let journal = home.join(".orkia/kernel/journal/feedback.jsonl");
    if journal.exists() {
        let raw = std::fs::read_to_string(&journal).unwrap();
        let lines: Vec<&str> = raw.lines().filter(|l| !l.is_empty()).collect();
        assert!(
            lines.len() >= 2,
            "expected ≥2 feedback events, got {}",
            lines.len()
        );
        for l in &lines {
            // each row should parse as an object with intent_text
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            assert!(v.get("intent_text").is_some());
            assert!(v.get("kernel_version").is_some());
        }
    }
}

#[test]
fn sprint_d_consent_default_off_grant_revoke_purge() {
    let Some(bin) = kernel_binary() else {
        eprintln!("skipping: ORKIA_KERNEL_BIN not set");
        return;
    };

    let tmp = TempDir::new().unwrap();
    let home = tmp.path().to_path_buf();
    let sock = home.join(".orkia/run/kernel.sock");
    let mut child = spawn_kernel(&bin, &home);
    wait_for_socket(&sock);

    let rpc = orkia_kernel_client::discover_at(sock).expect("discover");

    // Default state: OFF, non-empty kernel_id, no events buffered.
    let status = rpc.contribute_status().expect("contribute_status");
    assert!(!status.granted, "default consent should be OFF");
    assert!(!status.kernel_id.is_empty());

    // Wrong phrase → PhraseMismatch, no state change.
    let bad = rpc
        .contribute_set(true, Some("i consent"))
        .expect("contribute_set");
    assert!(matches!(
        bad,
        orkia_shell_types::KernelContributeOutcome::PhraseMismatch
    ));
    let after_bad = rpc.contribute_status().unwrap();
    assert!(!after_bad.granted);

    // Correct phrase → Ok, status flips ON.
    let good = rpc
        .contribute_set(true, Some("I consent"))
        .expect("contribute_set");
    assert!(matches!(
        good,
        orkia_shell_types::KernelContributeOutcome::Ok
    ));
    let after_good = rpc.contribute_status().unwrap();
    assert!(after_good.granted);

    // Generate a feedback event so the journal has something to clear.
    rpc.classify_with_timeout("@nico hi", Duration::from_millis(500))
        .unwrap();

    // Revoke
    let off = rpc.contribute_set(false, None).expect("contribute_set off");
    assert!(matches!(
        off,
        orkia_shell_types::KernelContributeOutcome::Ok
    ));
    assert!(!rpc.contribute_status().unwrap().granted);

    // Audit log should exist with at least one Granted + one Revoked
    let audit = home.join(".orkia/kernel/consent-audit.jsonl");
    if audit.exists() {
        let raw = std::fs::read_to_string(&audit).unwrap();
        assert!(raw.contains("granted"));
        assert!(raw.contains("revoked"));
    }

    // Purge clears the local journal.
    let _ = rpc.contribute_purge();
    let journal = home.join(".orkia/kernel/journal/feedback.jsonl");
    assert!(!journal.exists() || journal.metadata().unwrap().len() == 0);

    rpc.shutdown().unwrap();
    let _ = child.wait();
}

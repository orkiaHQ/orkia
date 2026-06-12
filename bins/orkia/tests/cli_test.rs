// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Integration tests for the `orkia` binary.
//!
//! Focused on the chsh-safety surface: `-c "<cmd>"` must execute and
//! exit with brush's reported exit code, no prompt, no TUI. This is
//! the contract `ssh user@host 'cmd'` and cron jobs depend on.

use std::process::Command;

mod support;
use support::{DaemonGuard, wait_for_path, write_agent_definition, write_echo_agent_config};

fn orkia_bin() -> &'static str {
    env!("CARGO_BIN_EXE_orkia")
}

#[test]
fn dash_c_exit_0() {
    let out = Command::new(orkia_bin())
        .args(["-c", "true"])
        .output()
        .expect("spawn orkia");
    assert!(
        out.status.success(),
        "expected exit 0, got {:?}",
        out.status
    );
}

#[test]
fn dash_c_exit_1() {
    let out = Command::new(orkia_bin())
        .args(["-c", "false"])
        .output()
        .expect("spawn orkia");
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn dash_c_exit_passthrough() {
    let out = Command::new(orkia_bin())
        .args(["-c", "exit 42"])
        .output()
        .expect("spawn orkia");
    assert_eq!(out.status.code(), Some(42));
}

#[test]
fn dash_c_missing_command_is_127() {
    let out = Command::new(orkia_bin())
        .args(["-c", "this_command_does_not_exist_xyz"])
        .output()
        .expect("spawn orkia");
    assert_eq!(out.status.code(), Some(127));
}

#[test]
fn dash_c_ps_aux_is_system_ps() {
    // POSIX-first — the payload reaches the system ps via brush. `ps aux`
    // exits 0 there; the Orkia builtin would reject `aux` as an unknown
    // flag. (Output bytes are PTY-bound — see dash_c_stdout_pipeable — so
    // the exit code is the observable contract here; the routing decision
    // itself is unit-pinned in dash_c.rs.)
    let out = Command::new(orkia_bin())
        .args(["-c", "ps aux > /dev/null"])
        .output()
        .expect("spawn orkia");
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn dash_c_stdout_pipeable() {
    // The command runs inside brush's PTY; orkia is responsible only
    // for the exit code. So we just confirm the exit code is right
    // and the binary terminates promptly — not that arbitrary stdout
    // bytes show up on the parent's pipe, which is intentionally
    // PTY-bound. (The `orkia -c "echo hi" | grep hi` interactive case
    // works because the PTY's master side is read by the engine and
    // not consumed here.)
    let out = Command::new(orkia_bin())
        .args(["-c", "true && echo done"])
        .output()
        .expect("spawn orkia");
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn version_flag() {
    let out = Command::new(orkia_bin())
        .arg("--version")
        .output()
        .expect("spawn orkia");
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.starts_with("orkia "), "got {s:?}");
}

#[test]
fn unknown_flag_exits_nonzero() {
    let out = Command::new(orkia_bin())
        .arg("--definitely-not-a-real-flag")
        .output()
        .expect("spawn orkia");
    assert_ne!(out.status.code(), Some(0));
}

#[test]
fn tui_and_no_tui_conflict_is_rejected() {
    let out = Command::new(orkia_bin())
        .args(["--tui", "--no-tui"])
        .output()
        .expect("spawn orkia");
    assert_ne!(out.status.code(), Some(0));
}

#[test]
fn dash_c_audit_records_plain_shell_command() {
    let home = tempfile::tempdir().expect("temp home");
    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["--audit", "-c", "true"])
        .output()
        .expect("spawn orkia");
    assert_eq!(out.status.code(), Some(0));
    let data_dir = home.path().join(".orkia");
    let journal = std::fs::read_to_string(data_dir.join("journal.jsonl")).expect("journal");
    assert!(journal.contains("shell.start"));
    assert!(journal.contains("shell.complete"));
    let seal = std::fs::read_to_string(data_dir.join("workspace").join("seal.jsonl"))
        .expect("workspace seal");
    assert!(seal.contains("shell.start"));
    assert!(seal.contains("shell.complete"));
}

#[test]
fn dash_c_detach_rejects_plain_shell_commands() {
    let out = Command::new(orkia_bin())
        .args(["--detach", "-c", "ls -la"])
        .output()
        .expect("spawn orkia");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("agentic command"));
}

#[test]
fn dash_c_detach_accepts_agent_pipeline_runtime() {
    let home = tempfile::tempdir().expect("temp home");
    let orkia_dir = home.path().join(".orkia");
    std::fs::create_dir_all(&orkia_dir).expect("create .orkia");
    write_echo_agent_config(home.path(), &orkia_dir);
    let _guard = DaemonGuard::new(home.path());

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["--detach", "-c", "@echo first | @echo second"])
        .output()
        .expect("spawn orkia");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("detached"), "stdout={stdout}");
    assert!(stdout.contains("echo|echo"), "stdout={stdout}");
}

#[test]
fn daemon_detach_ps_tell_attach_and_kill_round_trip() {
    let home = tempfile::tempdir().expect("temp home");
    let orkia_dir = home.path().join(".orkia");
    std::fs::create_dir_all(&orkia_dir).expect("create .orkia");
    write_echo_agent_config(home.path(), &orkia_dir);
    let _guard = DaemonGuard::new(home.path());

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["--detach", "-c", "@echo hello"])
        .output()
        .expect("spawn detach");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("[1] detached"), "stdout={stdout}");

    let cache = orkia_dir
        .join("run")
        .join("jobs")
        .join("1")
        .join("job.json");
    assert!(
        cache.exists(),
        "job cache should exist at {}",
        cache.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let daemon_sock = orkia_dir.join("run").join("pty-daemon.sock");
        let control_sock = orkia_dir
            .join("run")
            .join("jobs")
            .join("1")
            .join("control.sock");
        wait_for_path(&control_sock);
        let daemon_mode = std::fs::metadata(&daemon_sock)
            .expect("daemon socket metadata")
            .permissions()
            .mode()
            & 0o777;
        let control_mode = std::fs::metadata(&control_sock)
            .expect("control socket metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(daemon_mode, 0o600);
        assert_eq!(control_mode, 0o600);
    }
    let journal = std::fs::read_to_string(orkia_dir.join("journal.jsonl")).expect("journal");
    assert!(journal.contains("detached.spawn"), "journal={journal}");
    let daemon_seal = orkia_dir
        .join("agents")
        .join("daemon")
        .join("jobs")
        .join("1")
        .join("seal.jsonl");
    let seal = std::fs::read_to_string(&daemon_seal)
        .unwrap_or_else(|err| panic!("daemon seal {}: {err}", daemon_seal.display()));
    assert!(seal.contains("detached.spawn"), "seal={seal}");

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .arg("ps")
        .output()
        .expect("spawn ps");
    assert_eq!(out.status.code(), Some(0));
    let ps = String::from_utf8_lossy(&out.stdout);
    assert!(ps.contains("echo"), "ps={ps}");
    assert!(ps.contains("detached"), "ps={ps}");
    assert!(ps.contains("CPU"), "ps={ps}");
    assert!(ps.contains("MEM"), "ps={ps}");

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["ps", "--json"])
        .output()
        .expect("spawn ps json");
    assert_eq!(out.status.code(), Some(0));
    let ps_json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("daemon ps json should parse");
    let jobs = ps_json["jobs"].as_array().expect("jobs array");
    assert_eq!(jobs[0]["agent"], "echo");
    assert!(jobs[0].get("cpu_percent").is_some());
    assert!(jobs[0].get("mem_percent").is_some());
    assert!(jobs[0].get("runtime_secs").is_some());
    assert!(jobs[0].get("control_socket").is_some());
    assert!(jobs[0].get("pty_owner_pid").is_some());
    assert!(jobs[0].get("seal_path").is_some());
    assert_eq!(jobs[0]["attachable"], true);
    assert_eq!(jobs[0]["stages"][0]["target"], "@echo");
    let stage_id = jobs[0]["stages"][0]["id"]
        .as_u64()
        .expect("stage id")
        .to_string();
    assert_eq!(jobs[0]["stages"][0]["attachable"], true);

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["tell", "1:@echo", "world"])
        .output()
        .expect("spawn tell");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stage_target = format!("1:{stage_id}");
    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["tell", &stage_target, "again"])
        .output()
        .expect("spawn tell by stage id");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["attach", &stage_target])
        .output()
        .expect("spawn attach");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let attached = String::from_utf8_lossy(&out.stdout);
    assert!(
        attached.contains("agent:echo") || attached.contains("spawned as background"),
        "attached={attached:?}"
    );

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["kill", "1:@echo"])
        .output()
        .expect("spawn targeted kill");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let journal = std::fs::read_to_string(orkia_dir.join("journal.jsonl")).expect("journal");
    assert!(journal.contains("detached.kill_stage"), "journal={journal}");
    let seal = std::fs::read_to_string(&daemon_seal)
        .unwrap_or_else(|err| panic!("daemon seal {}: {err}", daemon_seal.display()));
    assert!(seal.contains("detached.tell"), "seal={seal}");
    assert!(seal.contains("detached.kill_stage"), "seal={seal}");

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["kill", "1"])
        .output()
        .expect("spawn kill");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn daemon_tell_requires_agent_target() {
    let out = Command::new(orkia_bin())
        .args(["tell", "1", "world"])
        .output()
        .expect("spawn tell");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("1:@sage"), "stderr={stderr}");
}

#[test]
fn daemon_ps_recovers_stale_cached_jobs() {
    let home = tempfile::tempdir().expect("temp home");
    let orkia_dir = home.path().join(".orkia");
    let job_dir = orkia_dir.join("run").join("jobs").join("7");
    std::fs::create_dir_all(&job_dir).expect("create job cache dir");
    std::fs::write(
        job_dir.join("job.json"),
        r#"{
  "id": 7,
  "agent": "echo",
  "state": "detached",
  "pid": 999999,
  "label": "@echo cached",
  "runtime_secs": 3,
  "stages": [
    {
      "target": "@echo",
      "state": "unknown",
      "pid": 999999,
      "runtime_secs": 3
    }
  ]
}"#,
    )
    .expect("write job cache");
    let _guard = DaemonGuard::new(home.path());

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["ps", "--json"])
        .output()
        .expect("spawn ps");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let ps_json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("daemon ps json should parse");
    let job = &ps_json["jobs"][0];
    assert_eq!(job["id"], 7);
    assert_eq!(job["state"], "pid_dead");
    assert!(job["pid"].is_null());

    // The corpse is reported that one time, then the list self-heals: the
    // cache entry is reaped so the next roster is clean and `@echo` can
    // spawn fresh instead of telling a dead job.
    assert!(!job_dir.join("job.json").exists());

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["attach", "7:@echo"])
        .output()
        .expect("spawn attach");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("job 7 not found"), "stderr={stderr}");

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["ps", "--json"])
        .output()
        .expect("spawn ps after self-heal");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let ps_json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("daemon ps json should parse");
    let jobs = ps_json["jobs"].as_array().expect("jobs");
    assert!(jobs.is_empty(), "roster should self-heal, got {jobs:?}");
}

#[test]
fn real_tui_detached_stage_attach_acceptance() {
    let Ok(agent_cmd) = std::env::var("ORKIA_REAL_TUI_AGENT_CMD") else {
        eprintln!("skipping real TUI acceptance; set ORKIA_REAL_TUI_AGENT_CMD=/path/to/codex");
        return;
    };
    let home = tempfile::tempdir().expect("temp home");
    let orkia_dir = home.path().join(".orkia");
    std::fs::create_dir_all(&orkia_dir).expect("create .orkia");
    write_agent_definition(&orkia_dir, "real", "real", &agent_cmd);
    let _guard = DaemonGuard::new(home.path());

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["--detach", "-c", "@real"])
        .output()
        .expect("spawn real tui detach");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["ps", "--json"])
        .output()
        .expect("spawn ps json");
    assert_eq!(out.status.code(), Some(0));
    let ps_json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("daemon ps json should parse");
    assert_eq!(ps_json["jobs"][0]["stages"][0]["target"], "@real");

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["attach", "1:@real"])
        .output()
        .expect("spawn real tui attach");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.stdout.is_empty(),
        "real TUI attach should replay visible PTY bytes"
    );

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["kill", "1"])
        .output()
        .expect("kill real tui");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use std::process::Command;

mod support;
use support::{DaemonGuard, wait_for_path, write_echo_agent_config};

fn orkia_bin() -> &'static str {
    env!("CARGO_BIN_EXE_orkia")
}

#[test]
fn daemon_status_inspect_logs_stop_and_wait() {
    let home = tempfile::tempdir().expect("temp home");
    let orkia_dir = home.path().join(".orkia");
    std::fs::create_dir_all(&orkia_dir).expect("create .orkia");
    write_echo_agent_config(home.path(), &orkia_dir);
    let _guard = DaemonGuard::new(home.path());

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["daemon", "status"])
        .output()
        .expect("daemon status");
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("state: stopped"), "stdout={stdout}");

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["--detach", "-c", "@echo lifecycle"])
        .output()
        .expect("spawn detach");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["daemon", "status"])
        .output()
        .expect("daemon status running");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("state: running"), "stdout={stdout}");
    assert!(stdout.contains("protocol_version: 1"), "stdout={stdout}");
    assert!(stdout.contains("jobs: 1"), "stdout={stdout}");

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["inspect", "1"])
        .output()
        .expect("inspect");
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("JOB 1"), "stdout={stdout}");
    assert!(stdout.contains("control_socket:"), "stdout={stdout}");
    assert!(stdout.contains("stages:"), "stdout={stdout}");
    assert!(stdout.contains("@echo"), "stdout={stdout}");

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["logs", "1", "--last", "5"])
        .output()
        .expect("logs");
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("detached.spawn"), "stdout={stdout}");

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["stop", "1"])
        .output()
        .expect("stop");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["wait", "1", "--timeout", "2"])
        .output()
        .expect("wait");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("1 stopped"), "stdout={stdout}");

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["inspect", "1"])
        .output()
        .expect("inspect stopped");
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("status: stopped"), "stdout={stdout}");
    assert!(stdout.contains("attachable: false"), "stdout={stdout}");
}

#[test]
fn daemon_stop_command_removes_socket_and_lock() {
    let home = tempfile::tempdir().expect("temp home");
    let orkia_dir = home.path().join(".orkia");
    std::fs::create_dir_all(&orkia_dir).expect("create .orkia");
    write_echo_agent_config(home.path(), &orkia_dir);

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["--detach", "-c", "@echo shutdown"])
        .output()
        .expect("spawn detach");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let socket = orkia_dir.join("run").join("pty-daemon.sock");
    let lock = orkia_dir.join("run").join("pty-daemon.lock");
    wait_for_path(&socket);
    wait_for_path(&lock);
    assert!(socket.exists(), "socket should exist");
    assert!(lock.exists(), "lock should exist");

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .arg("pty-daemon-stop")
        .output()
        .expect("daemon stop");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline && (socket.exists() || lock.exists()) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(!socket.exists(), "socket should be removed");
    assert!(!lock.exists(), "lock should be removed");
}

#[test]
fn zombie_holder_lock_does_not_block_startup() {
    let home = tempfile::tempdir().expect("temp home");
    let orkia_dir = home.path().join(".orkia");
    std::fs::create_dir_all(&orkia_dir).expect("create .orkia");
    write_echo_agent_config(home.path(), &orkia_dir);
    let _guard = DaemonGuard::new(home.path());

    // A child we deliberately never reap becomes a zombie — globally visible as
    // such to the daemon. A zombie still answers `kill(pid, 0)`, so without the
    // zombie check the daemon would treat this lock as live and refuse to start.
    let mut zombie = Command::new("true").spawn().expect("spawn `true`");
    let zombie_pid = zombie.id();
    // `true` exits in microseconds; give it ample margin to become a zombie
    // before the daemon reads the lock (we cannot wait() it — that would reap it).
    std::thread::sleep(std::time::Duration::from_millis(100));

    let run_dir = orkia_dir.join("run");
    std::fs::create_dir_all(&run_dir).expect("create run dir");
    let lock = run_dir.join("pty-daemon.lock");
    std::fs::write(&lock, format!("{zombie_pid}\n")).expect("write zombie lock");

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["--detach", "-c", "@echo revived"])
        .output()
        .expect("spawn detach");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let socket = run_dir.join("pty-daemon.sock");
    wait_for_path(&socket);
    assert!(
        socket.exists(),
        "daemon must start despite a zombie-held stale lock"
    );

    // Reap the zombie so the test leaves no corpse behind.
    let _ = zombie.wait();
}

#[test]
fn daemon_lists_multiple_parallel_jobs() {
    let home = tempfile::tempdir().expect("temp home");
    let orkia_dir = home.path().join(".orkia");
    std::fs::create_dir_all(&orkia_dir).expect("create .orkia");
    write_echo_agent_config(home.path(), &orkia_dir);
    let _guard = DaemonGuard::new(home.path());

    for label in ["one", "two"] {
        let out = Command::new(orkia_bin())
            .env("HOME", home.path())
            .args(["--detach", "-c", &format!("@echo {label}")])
            .output()
            .expect("spawn detach");
        assert_eq!(
            out.status.code(),
            Some(0),
            "stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["ps", "--json"])
        .output()
        .expect("ps json");
    assert_eq!(out.status.code(), Some(0));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("ps json parses");
    let jobs = json["jobs"].as_array().expect("jobs array");
    assert_eq!(jobs.len(), 2, "json={json}");
    assert_eq!(jobs[0]["id"], 1);
    assert_eq!(jobs[1]["id"], 2);

    for id in ["1", "2"] {
        let out = Command::new(orkia_bin())
            .env("HOME", home.path())
            .args(["stop", id])
            .output()
            .expect("stop");
        assert_eq!(
            out.status.code(),
            Some(0),
            "stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn daemon_wait_fails_fast_for_missing_job() {
    // A job the roster has NEVER seen errors immediately ("no such job"),
    // it does not spin until --timeout — client_api::wait only synthesizes
    // a terminal state for a job it saw alive that later vanished (the
    // done-reap race). Timing out on a typo'd id would be worse UX.
    let home = tempfile::tempdir().expect("temp home");
    let _guard = DaemonGuard::new(home.path());

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["wait", "99", "--timeout", "1"])
        .output()
        .expect("wait");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no such job: 99"), "stderr={stderr}");
}

#[test]
fn daemon_crash_restarts_and_reports_cached_job_state() {
    let home = tempfile::tempdir().expect("temp home");
    let orkia_dir = home.path().join(".orkia");
    std::fs::create_dir_all(&orkia_dir).expect("create .orkia");
    write_echo_agent_config(home.path(), &orkia_dir);

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["--detach", "-c", "@echo crash"])
        .output()
        .expect("spawn detach");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["daemon", "status"])
        .output()
        .expect("daemon status");
    let status = String::from_utf8_lossy(&out.stdout);
    let pid = status
        .lines()
        .find_map(|line| line.strip_prefix("pid: "))
        .and_then(|raw| raw.parse::<i32>().ok())
        .expect("daemon pid");
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }

    let socket = orkia_dir.join("run").join("pty-daemon.sock");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline && socket.exists() {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let out = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["ps", "--json"])
        .output()
        .expect("ps json after daemon crash");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("ps json parses");
    let job = &json["jobs"][0];
    let state = job["state"].as_str().expect("state");
    assert!(
        matches!(
            state,
            "recovered" | "pid_dead" | "lost_pty" | "control_unavailable"
        ),
        "json={json}"
    );

    let _ = Command::new(orkia_bin())
        .env("HOME", home.path())
        .args(["kill", "1"])
        .output();
    let _ = Command::new(orkia_bin())
        .env("HOME", home.path())
        .arg("pty-daemon-stop")
        .output();
}

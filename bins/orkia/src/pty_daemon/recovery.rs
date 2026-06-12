// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use std::os::unix::net::UnixStream;

use super::protocol::{DaemonJobInfo, write_error, write_ok};
use super::{DaemonState, audit, runtime_control};

pub(super) fn merge_cached_jobs(jobs: &mut Vec<DaemonJobInfo>, state: &DaemonState) {
    let live: std::collections::HashSet<u32> = jobs.iter().map(|job| job.id).collect();
    for mut cached in audit::read_job_caches(&state.data_dir) {
        if live.contains(&cached.id) {
            continue;
        }
        if is_cached_terminal(&cached.state) {
            // Bash-style notification: a finished job is reported once,
            // then leaves the roster — `ps` must not accumulate done jobs
            // forever. Reap on this list so the entry is returned one last
            // time and the next list is clean. Only "done" is reaped:
            // `stop` promises to keep cache/logs, and "failed" stays
            // visible until an explicit `gc` so failures aren't missed.
            if cached.state == "done" {
                reap_cached_job(&state.data_dir, cached.id);
            }
            jobs.push(cached);
            continue;
        }
        let control_sock = runtime_control::control_socket_path(&state.data_dir, cached.id);
        if let Some(stages) = runtime_control::list(&control_sock) {
            cached.state = "recovered".to_string();
            cached.stages = stages;
        } else {
            mark_unreachable(&mut cached);
            // The runtime is gone (pid dead / no control socket left): the
            // cache entry is a corpse. Reap it now so the roster self-heals —
            // the entry is still returned this one time, marked dead, so `ps`
            // shows what happened, but the next list no longer carries it and
            // `@agent` dispatch can spawn fresh instead of telling a dead job.
            // `lost_pty` (process alive, control unreachable) is NOT reaped:
            // that is an anomaly to surface, not a corpse to clean.
            if matches!(cached.state.as_str(), "pid_dead" | "control_unavailable") {
                reap_cached_job(&state.data_dir, cached.id);
            }
        }
        jobs.push(cached);
    }
    jobs.sort_by_key(|job| job.id);
}

pub(super) fn gc_cached_jobs(state: &DaemonState) -> Vec<DaemonJobInfo> {
    let live: std::collections::HashSet<u32> = state.jobs.keys().copied().collect();
    let mut removed = Vec::new();
    for mut job in audit::read_job_caches(&state.data_dir) {
        if live.contains(&job.id) {
            continue;
        }
        if !is_cached_terminal(&job.state) {
            mark_unreachable(&mut job);
        }
        if matches!(
            job.state.as_str(),
            "done" | "stopped" | "pid_dead" | "control_unavailable"
        ) {
            reap_cached_job(&state.data_dir, job.id);
            removed.push(job);
        }
    }
    removed.sort_by_key(|job| job.id);
    removed
}

/// Remove a dead job's cache entry and run directory, and record the
/// reap in the daemon audit trail. Shared by the explicit `gc` request
/// and the list-time self-heal in [`merge_cached_jobs`].
fn reap_cached_job(data_dir: &std::path::Path, id: u32) {
    audit::remove_job_cache(data_dir, id);
    let dir = data_dir.join("run").join("jobs").join(id.to_string());
    let _ = std::fs::remove_file(dir.join("control.sock"));
    // The runtime's per-job hub socket; left behind on a hard kill and
    // would keep `remove_dir` failing forever.
    let _ = std::fs::remove_file(dir.join("agent.sock"));
    let _ = std::fs::remove_dir(&dir);
    audit::emit_event(data_dir, "detached.gc", id, None, None);
}

fn is_cached_terminal(state: &str) -> bool {
    matches!(state, "done" | "stopped" | "failed")
}

pub(super) fn handle_tell(
    mut stream: UnixStream,
    state: &DaemonState,
    id: u32,
    target: String,
    message: String,
) {
    let Some(job) = audit::read_job_cache(&state.data_dir, id) else {
        let _ = write_error(&mut stream, format!("job {id} not found"));
        return;
    };
    if !job_accepts_target(&job, &target) {
        let _ = write_error(
            &mut stream,
            format!("job {id} has no target {target}; available: {}", job.agent),
        );
        return;
    }
    let control_sock = runtime_control::control_socket_path(&state.data_dir, id);
    match runtime_control::tell_with_retry(&control_sock, &target, &message) {
        Ok(()) => {
            audit::emit_event(
                &state.data_dir,
                "detached.tell",
                id,
                Some(target.as_str()),
                None,
            );
            let _ = write_ok(&mut stream);
        }
        Err(err) => write_stale_error(&mut stream, id, err),
    }
}

pub(super) fn handle_kill(mut stream: UnixStream, state: &DaemonState, id: u32) {
    let Some(job) = audit::read_job_cache(&state.data_dir, id) else {
        let _ = write_error(&mut stream, format!("job {id} not found"));
        return;
    };
    if let Some(pid) = job.pid {
        let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                audit::remove_job_cache(&state.data_dir, id);
                audit::emit_event(&state.data_dir, "detached.kill_stale", id, None, None);
                let _ = write_ok(&mut stream);
                return;
            }
            let _ = write_error(&mut stream, format!("kill recovered job {id}: {err}"));
            return;
        }
    }
    audit::remove_job_cache(&state.data_dir, id);
    audit::emit_event(&state.data_dir, "detached.kill", id, None, None);
    let _ = write_ok(&mut stream);
}

pub(super) fn handle_kill_target(
    mut stream: UnixStream,
    state: &DaemonState,
    id: u32,
    target: String,
) {
    let Some(job) = audit::read_job_cache(&state.data_dir, id) else {
        let _ = write_error(&mut stream, format!("job {id} not found"));
        return;
    };
    if !job_accepts_target(&job, &target) {
        let _ = write_error(
            &mut stream,
            format!("job {id} has no target {target}; available: {}", job.agent),
        );
        return;
    }
    let control_sock = runtime_control::control_socket_path(&state.data_dir, id);
    match runtime_control::kill_with_retry(&control_sock, &target) {
        Ok(()) => {
            audit::emit_event(
                &state.data_dir,
                "detached.kill_stage",
                id,
                Some(target.as_str()),
                None,
            );
            let _ = write_ok(&mut stream);
        }
        Err(err) => write_stale_error(&mut stream, id, err),
    }
}

fn mark_unreachable(job: &mut DaemonJobInfo) {
    let state = match job.pid {
        Some(pid) if process_exists(pid) => "lost_pty",
        Some(_) => "pid_dead",
        None => "control_unavailable",
    };
    job.state = state.to_string();
    job.lost_reason = Some(state.to_string());
    job.attachable = false;
    if state == "pid_dead" {
        job.pid = None;
    }
    for stage in &mut job.stages {
        stage.state = state.to_string();
        stage.lost_reason = Some(state.to_string());
        stage.attachable = false;
        if state == "pid_dead" {
            stage.pid = None;
        }
    }
}

fn process_exists(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn job_accepts_target(job: &DaemonJobInfo, target: &str) -> bool {
    if let Ok(stage_id) = target.parse::<u32>() {
        return job.stages.iter().any(|stage| stage.id == stage_id);
    }
    job.agent
        .split('|')
        .any(|name| name.strip_prefix('@').unwrap_or(name) == target)
}

fn write_stale_error(stream: &mut UnixStream, id: u32, err: String) {
    let _ = write_error(
        stream,
        format!("job {id} is stale; runtime control unavailable ({err})"),
    );
}

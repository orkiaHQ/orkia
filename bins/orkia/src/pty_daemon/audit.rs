// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use std::path::Path;

use orkia_shell_types::job::JobId;
use orkia_shell_types::journal::types::{EventType, JournalEnvelope};

use super::DaemonJobInfo;

pub(super) fn emit_event(
    data_dir: &Path,
    event: &str,
    id: u32,
    target: Option<&str>,
    command: Option<&str>,
) {
    let mut env = JournalEnvelope::now(EventType::Shell);
    env.source = Some("orkia-daemon".to_string());
    env.event = Some(event.to_string());
    env.job_id = Some(id);
    if let Some(target) = target {
        env.extra.insert(
            "target".to_string(),
            serde_json::Value::String(target.to_string()),
        );
    }
    if let Some(command) = command {
        env.extra.insert(
            "command".to_string(),
            serde_json::Value::String(command.to_string()),
        );
    }
    let mut journal = orkia_shell::journal::store::JournalStore::new(data_dir);
    journal.append(&env);
    seal_event(data_dir, event, id, target, command);
}

pub(super) fn write_job_cache(data_dir: &Path, job: &DaemonJobInfo) {
    let dir = data_dir.join("run").join("jobs").join(job.id.to_string());
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(bytes) = serde_json::to_vec_pretty(job) else {
        return;
    };
    let _ = std::fs::write(dir.join("job.json"), bytes);
}

pub(super) fn remove_job_cache(data_dir: &Path, id: u32) {
    let dir = data_dir.join("run").join("jobs").join(id.to_string());
    let _ = std::fs::remove_file(dir.join("job.json"));
}

pub(super) fn read_job_cache(data_dir: &Path, id: u32) -> Option<DaemonJobInfo> {
    let path = data_dir
        .join("run")
        .join("jobs")
        .join(id.to_string())
        .join("job.json");
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub(super) fn read_job_caches(data_dir: &Path) -> Vec<DaemonJobInfo> {
    let root = data_dir.join("run").join("jobs");
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut jobs: Vec<_> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let bytes = std::fs::read(entry.path().join("job.json")).ok()?;
            serde_json::from_slice::<DaemonJobInfo>(&bytes).ok()
        })
        .collect();
    jobs.sort_by_key(|job| job.id);
    jobs
}

pub(super) fn read_job_logs(data_dir: &Path, job: &DaemonJobInfo, limit: usize) -> Vec<String> {
    let path = job
        .seal_path
        .as_deref()
        .map(|p| data_dir.join(p))
        .unwrap_or_else(|| daemon_seal_path(data_dir, job.id));
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut lines: Vec<String> = text.lines().map(ToString::to_string).collect();
    if lines.len() > limit {
        lines.drain(0..lines.len() - limit);
    }
    lines
}

pub(super) fn next_cached_job_id(data_dir: &Path) -> u32 {
    read_job_caches(data_dir)
        .into_iter()
        .map(|job| job.id)
        .max()
        .and_then(|id| id.checked_add(1))
        .unwrap_or(1)
}

fn seal_event(data_dir: &Path, event: &str, id: u32, target: Option<&str>, command: Option<&str>) {
    let detail = serde_json::json!({
        "daemon_job_id": id,
        "target": target,
        "command": command,
        "origin": "orkia-daemon",
    });
    let mut manager = orkia_shell::SealManager::new(data_dir.to_path_buf());
    let job_id = JobId(id);
    // Genesis (archiving any stale chain from a re-used id) ONLY at
    // spawn; every later event must CONTINUE the chain — a fresh
    // manager is built per event, so `create_job_chain` here would
    // archive the live chain each time, leaving one record per file.
    if event == "detached.spawn" {
        manager.create_job_chain(job_id, "daemon");
    } else {
        manager.open_job_chain(job_id, "daemon");
    }
    let _ = manager.seal_job(job_id, event, detail.clone());
    let _ = manager.seal_workspace(event, detail);
    if matches!(
        event,
        "detached.kill" | "detached.kill_stale" | "detached.stop" | "detached.complete"
    ) {
        let _ = manager.close_job_chain(job_id);
    }
}

fn daemon_seal_path(data_dir: &Path, id: u32) -> std::path::PathBuf {
    data_dir
        .join("agents")
        .join("daemon")
        .join("jobs")
        .join(id.to_string())
        .join("seal.jsonl")
}

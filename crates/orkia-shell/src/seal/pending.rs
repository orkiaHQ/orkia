// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! When crond fires `orkia -c "@agent ..."` it exports
//! `ORKIA_SCHEDULED=1`. If the target agent's `agent.toml` carries
//! `[trust] approval = "required"`, the `orkia every` builtin also
//! exports `ORKIA_SCHEDULED_APPROVAL=required` in the crontab line.
//! On `agent.complete` / `agent.failed` the consumer routes through
//! the helpers in this module:
//!
//! * approval-required successes → `~/.orkia/pending/<job-id>.json`
//!   parked + `approval_pending` journal event.
//! * any scheduled failure (non-zero exit) → `scheduled_failure`
//!   journal event with `exit_code`.
//!
//! Both writes are best-effort: they emit a `tracing::warn!` on
//! failure but never crash the seal consumer. Audit-trail records
//! still land via SEAL even if these auxiliary writes fail.

use std::fs;
use std::path::{Path, PathBuf};

use orkia_shell_types::job::JobId;
use serde_json::json;

/// Captures the "is this a cron-fired orkia?" decision plus the
/// approval-required flag once, at process startup. We snapshot the
/// env into a struct instead of reading it per-event so tests can
/// inject deterministic state without mutating process env (which is
/// notoriously racy under parallel test execution).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScheduledContext {
    pub is_scheduled: bool,
    pub approval_required: bool,
}

impl ScheduledContext {
    /// Build from the current process env. Called once at consumer
    /// spawn; the runtime then carries this snapshot around instead
    /// of touching `std::env` again.
    pub fn from_env() -> Self {
        let is_scheduled = matches!(std::env::var("ORKIA_SCHEDULED").as_deref(), Ok("1"));
        let approval_required = matches!(
            std::env::var("ORKIA_SCHEDULED_APPROVAL").as_deref(),
            Ok("required") | Ok("require"),
        );
        Self {
            is_scheduled,
            approval_required,
        }
    }
}

/// Write `~/.orkia/pending/<job_id>.json`. Holds enough context for an
/// interactive REPL session to surface a notification and let the user
/// approve/deny the parked result. Overwrites any prior file with the
/// same job id (which shouldn't happen — job ids are monotonic).
pub fn park_scheduled_result(
    data_dir: &Path,
    job_id: JobId,
    agent: &str,
    exit_code: Option<i32>,
) -> std::io::Result<PathBuf> {
    let dir = data_dir.join("pending");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", job_id.0));
    let payload = json!({
        "job_id": job_id.0,
        "agent": agent,
        "exit_code": exit_code,
        "origin": "scheduled",
        "parked_at": chrono::Utc::now().to_rfc3339(),
        "cron_expression": std::env::var("ORKIA_SCHEDULED_CRON").ok(),
    });
    fs::write(&path, serde_json::to_vec_pretty(&payload)?)?;
    Ok(path)
}

/// Append a single NDJSON line to `~/.orkia/journal.jsonl`. We don't
/// hold a `JournalStore` handle inside the seal consumer (the store is
/// REPL-owned), so the seal consumer writes directly here. The on-disk
/// format is the same one `JournalStore::append` produces.
pub fn append_journal_event(
    data_dir: &Path,
    event_type: &str,
    event: &str,
    job_id: JobId,
    agent: &str,
    extra: serde_json::Value,
) {
    use std::io::Write;
    let path = data_dir.join("journal.jsonl");
    if let Some(parent) = path.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        tracing::warn!(?path, error = %e, "pending: failed to create journal dir");
        return;
    }
    let envelope = json!({
        "type": event_type,
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "job_id": job_id.0,
        "agent": agent,
        "event": event,
        // Inline extras so consumers can filter without re-parsing.
        "origin": "scheduled",
        "extra": extra,
    });
    let line = match serde_json::to_string(&envelope) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "pending: failed to serialise journal envelope");
            return;
        }
    };
    let mut file = match fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(?path, error = %e, "pending: failed to open journal file");
            return;
        }
    };
    if let Err(e) = writeln!(file, "{line}") {
        tracing::warn!(?path, error = %e, "pending: failed to append journal line");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn park_writes_pending_json_with_origin() {
        let dir = tempdir().unwrap();
        let path = park_scheduled_result(dir.path(), JobId(7), "faye", Some(0)).unwrap();
        assert!(path.exists());
        let body = fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["job_id"], 7);
        assert_eq!(v["agent"], "faye");
        assert_eq!(v["origin"], "scheduled");
        assert_eq!(v["exit_code"], 0);
    }

    #[test]
    fn append_journal_writes_ndjson_line() {
        let dir = tempdir().unwrap();
        append_journal_event(
            dir.path(),
            "approval",
            "approval_pending",
            JobId(9),
            "sage",
            json!({ "command": "@sage check" }),
        );
        let body = fs::read_to_string(dir.path().join("journal.jsonl")).unwrap();
        let line = body.lines().next().expect("at least one line");
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["type"], "approval");
        assert_eq!(v["event"], "approval_pending");
        assert_eq!(v["origin"], "scheduled");
        assert_eq!(v["job_id"], 9);
    }
}

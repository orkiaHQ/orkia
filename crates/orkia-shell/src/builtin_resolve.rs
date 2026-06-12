// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use orkia_shell_types::{JobId, JobInfo, JobKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillAction {
    StopJob(JobId),
    SystemKill { target: String, signal: String },
}

///
/// Priority:
/// 1. If a signal flag was given (-9, -TERM, …), always passthrough to system kill.
/// 2. Numeric target that matches an orkia job id → stop that job.
/// 3. String target that matches an agent name → stop that agent's latest job.
/// 4. Numeric target that matches a known job's PID → stop that job.
/// 5. Fallback → system kill with SIGTERM.
pub fn resolve_kill(target: &str, signal: Option<&str>, jobs: &[JobInfo]) -> KillAction {
    if let Some(sig) = signal {
        // target so the signal path can dispatch back into the
        // orkia job table.
        if target.starts_with('%')
            && let Some(id) = resolve_job_target(target, jobs)
        {
            return KillAction::StopJob(id);
        }
        return KillAction::SystemKill {
            target: target.into(),
            signal: sig.into(),
        };
    }

    if target.starts_with('%')
        && let Some(id) = resolve_job_target(target, jobs)
    {
        return KillAction::StopJob(id);
    }

    if let Ok(job_id) = target.parse::<u32>()
        && jobs.iter().any(|j| j.id.0 == job_id)
    {
        return KillAction::StopJob(JobId(job_id));
    }

    if let Some(job) = jobs.iter().rev().find(|j| match &j.kind {
        JobKind::Agent { agent_name, .. } => agent_name == target,
        _ => false,
    }) {
        return KillAction::StopJob(job.id);
    }

    if let Ok(pid) = target.parse::<u32>()
        && let Some(job) = jobs.iter().find(|j| j.pid == Some(pid))
    {
        return KillAction::StopJob(job.id);
    }

    KillAction::SystemKill {
        target: target.into(),
        signal: "TERM".into(),
    }
}

/// Resolve a job target (used by `fg`, `bg`, `stop`, `attach`, `kill`).
///
/// Accepts:
/// - bash `%N` syntax: `%1` (numeric), `%+` (current = most recent), `%-`
///   (previous), `%string` (prefix-match on command label or agent name).
/// - bare numeric: job id, then PID fallback.
/// - bare string: agent name, latest job for that agent.
pub fn resolve_job_target(target: &str, jobs: &[JobInfo]) -> Option<JobId> {
    let parsed = orkia_shell_types::parse_job_target(target);
    // The daemon marker `[N]*` explicitly names a DAEMON job: decline so the
    // caller's daemon fallback owns it. (A plain `[N]` is bash's LOCAL display
    // and is resolved here.) Declining disambiguates a local/daemon id collision
    // toward the daemon job.
    if parsed.prefer_daemon {
        return None;
    }
    let target = parsed.core.as_str();
    // `%`-prefixed bash syntax for shell job control.
    if let Some(rest) = target.strip_prefix('%') {
        return resolve_pct(rest, jobs);
    }
    // `@faye` — the canonical agent-address form used everywhere else
    // (dispatch, pipelines) resolves like the bare name.
    let target = target.strip_prefix('@').unwrap_or(target);
    if let Ok(id) = target.parse::<u32>()
        && jobs.iter().any(|j| j.id.0 == id)
    {
        return Some(JobId(id));
    }
    if let Some(job) = jobs.iter().rev().find(|j| match &j.kind {
        JobKind::Agent { agent_name, .. } => agent_name == target,
        _ => false,
    }) {
        return Some(job.id);
    }
    if let Ok(pid) = target.parse::<u32>()
        && let Some(job) = jobs.iter().find(|j| j.pid == Some(pid))
    {
        return Some(job.id);
    }
    None
}

/// job (current), `%-` is the previous job, `%string` matches a label
/// prefix. Caller has already stripped the leading `%`.
fn resolve_pct(rest: &str, jobs: &[JobInfo]) -> Option<JobId> {
    if rest.is_empty() || rest == "+" || rest == "%" {
        // `%` / `%+` / `%%` → current job = most recent (jobs is
        // typically ordered by spawn id ascending; last is newest).
        return jobs.last().map(|j| j.id);
    }
    if rest == "-" {
        // `%-` → previous job. Second-most-recent.
        let n = jobs.len();
        return jobs.get(n.checked_sub(2)?).map(|j| j.id);
    }
    if let Ok(id) = rest.parse::<u32>()
        && jobs.iter().any(|j| j.id.0 == id)
    {
        return Some(JobId(id));
    }
    // `%string` — match the command label (shell jobs) or agent name
    // (agent jobs) as a prefix.
    jobs.iter().rev().find_map(|j| {
        let candidate: &str = match &j.kind {
            JobKind::Shell { cmd } => cmd.as_str(),
            JobKind::Agent { agent_name, .. } => agent_name.as_str(),
            JobKind::ForgeApp { app_name } => app_name.as_str(),
        };
        if candidate.starts_with(rest) {
            Some(j.id)
        } else {
            None
        }
    })
}

/// Split a raw `kill` argument list into (signal, target).
///
/// `kill -9 1234` → (Some("9"), "1234")
/// `kill -TERM 1234` → (Some("TERM"), "1234")
/// `kill 1234` → (None, "1234")
pub fn split_kill_args(args: &[String]) -> Result<(Option<String>, String), String> {
    let mut signal: Option<String> = None;
    let mut target: Option<String> = None;
    for arg in args {
        if let Some(rest) = arg.strip_prefix('-') {
            if rest.is_empty() {
                return Err("kill: empty signal flag".into());
            }
            signal = Some(rest.to_string());
        } else if target.is_none() {
            target = Some(arg.clone());
        } else {
            return Err(format!("kill: unexpected extra argument '{arg}'"));
        }
    }
    let target = target.ok_or_else(|| "kill: missing target".to_string())?;
    Ok((signal, target))
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_shell_types::JobState;
    use std::time::Duration;

    fn shell_job(id: u32, cmd: &str) -> JobInfo {
        JobInfo {
            id: JobId(id),
            kind: JobKind::Shell { cmd: cmd.into() },
            state: JobState::Running,
            label: cmd.into(),
            pid: Some(1000 + id),
            runtime: Duration::from_secs(0),
            sink: None,
        }
    }

    fn agent_job(id: u32, name: &str) -> JobInfo {
        JobInfo {
            id: JobId(id),
            kind: JobKind::Agent {
                agent_id: uuid::Uuid::nil(),
                agent_name: name.into(),
            },
            state: JobState::Running,
            label: format!("agent:{name}"),
            pid: Some(2000 + id),
            runtime: Duration::from_secs(0),
            sink: None,
        }
    }

    #[test]
    fn pct_n_matches_job_id() {
        let jobs = vec![shell_job(1, "make"), shell_job(2, "sleep 10")];
        assert_eq!(resolve_job_target("%1", &jobs), Some(JobId(1)));
        assert_eq!(resolve_job_target("%2", &jobs), Some(JobId(2)));
        assert_eq!(resolve_job_target("%9", &jobs), None);
    }

    #[test]
    fn pct_plus_is_current_job() {
        let jobs = vec![shell_job(1, "old"), shell_job(2, "new")];
        assert_eq!(resolve_job_target("%+", &jobs), Some(JobId(2)));
        assert_eq!(resolve_job_target("%", &jobs), Some(JobId(2)));
    }

    #[test]
    fn pct_minus_is_previous_job() {
        let jobs = vec![shell_job(1, "old"), shell_job(2, "new")];
        assert_eq!(resolve_job_target("%-", &jobs), Some(JobId(1)));
        // With only one job, `%-` returns None.
        let one = vec![shell_job(1, "only")];
        assert_eq!(resolve_job_target("%-", &one), None);
    }

    #[test]
    fn pct_string_matches_prefix() {
        let jobs = vec![shell_job(1, "make build"), agent_job(2, "faye")];
        assert_eq!(resolve_job_target("%mak", &jobs), Some(JobId(1)));
        assert_eq!(resolve_job_target("%fay", &jobs), Some(JobId(2)));
        assert_eq!(resolve_job_target("%nope", &jobs), None);
    }

    #[test]
    fn at_prefixed_agent_name_resolves() {
        let jobs = vec![shell_job(1, "make"), agent_job(2, "faye")];
        assert_eq!(resolve_job_target("@faye", &jobs), Some(JobId(2)));
        assert_eq!(resolve_job_target("faye", &jobs), Some(JobId(2)));
        assert_eq!(resolve_job_target("@nope", &jobs), None);
    }

    #[test]
    fn kill_with_pct_routes_to_stop_job() {
        let jobs = vec![shell_job(1, "make")];
        let action = resolve_kill("%1", None, &jobs);
        assert_eq!(action, KillAction::StopJob(JobId(1)));
        // `kill -9 %1` also dispatches to the job table.
        let action = resolve_kill("%1", Some("9"), &jobs);
        assert_eq!(action, KillAction::StopJob(JobId(1)));
    }

    #[test]
    fn rendered_local_id_round_trips_to_its_job() {
        // A local job renders `[N]` (bash) — feeding that back must resolve to it.
        let jobs = vec![shell_job(1, "make"), agent_job(2, "faye")];
        for id in [1u32, 2] {
            let rendered =
                orkia_shell_types::render_job_id(orkia_shell_types::JobOwner::Local, id, None);
            assert_eq!(rendered, format!("[{id}]"));
            assert_eq!(
                resolve_job_target(&rendered, &jobs),
                Some(JobId(id)),
                "rendered {rendered} must resolve back to job {id}",
            );
        }
    }

    #[test]
    fn starred_daemon_id_is_declined_locally() {
        // `[N]*` names a DAEMON job; the local resolver declines so the caller's
        // daemon fallback owns it — even when a local job shares the id. A plain
        // `[N]` (bash local display) still resolves locally.
        let jobs = vec![shell_job(1, "make")];
        assert_eq!(resolve_job_target("[1]*", &jobs), None);
        assert_eq!(resolve_job_target("[1:2]*", &jobs), None);
        assert_eq!(resolve_job_target("[1]", &jobs), Some(JobId(1)));
    }
}

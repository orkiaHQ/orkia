// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use std::fmt;
use std::time::Duration;

use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct JobId(pub u32);

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Who owns a job's lifecycle. Decided at construction (a job spawned into the
/// REPL's `JobController` is `Local`; one spawned into the orkia daemon is
/// `Daemon`) — NOT inferred from survival. The name is deliberately *ownership*,
/// not lifetime: today a `Local` job dies with the REPL and a `Daemon` job
/// survives `exit`, but if that coincidence ever changes, the owner a row was
/// built from stays true while a "lifetime" label would lie.
///
/// The owner drives the rendered id prefix so the survival contract is legible
/// at a glance — see [`render_job_id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobOwner {
    /// REPL-local (`JobController`): a shell job (`sleep 100 &`) or an in-process
    /// agent (operator-ask synthesis, sink-bound, native). Dies with the shell.
    Local,
    /// Daemon-owned: every `@agent` dispatched from the prompt, `--detach`, and
    /// pipeline stages. Survives `exit`.
    Daemon,
}

/// Render a job id for display. `[N]` = local (bash job-control notation,
/// dies with the REPL); `[N]*` = daemon (survives `exit`) — the trailing `*`
/// is the survival marker. `stage` is set only for a daemon pipeline stage →
/// `[N:M]*`.
///
/// `[N]` for local is deliberate: orkia is a bash replacement, and bash already
/// shows local jobs as `[N]` (referenced as `%N`). The daemon job is the novel
/// thing that outlives the shell, so it — not the local one — carries the extra
/// mark.
///
/// This is the SINGLE source of truth for a rendered job id. Every form it
/// produces MUST parse back through [`parse_job_target`] — the round-trip is
/// asserted in this module's tests, so a new display form cannot ship without a
/// matching parse path.
pub fn render_job_id(owner: JobOwner, id: u32, stage: Option<u32>) -> String {
    let base = match stage {
        Some(s) => format!("[{id}:{s}]"),
        None => format!("[{id}]"),
    };
    match owner {
        JobOwner::Local => base,
        JobOwner::Daemon => format!("{base}*"),
    }
}

/// A job target parsed from user input OR a rendered id. The single canonical
/// normalizer every targeting command (`kill`/`wait`/`fg`/`bg`/`stop`/
/// `disown`/`attach`) funnels through before resolution.
///
/// - `core` is what the job/daemon resolvers consume: surrounding `[ ]` (and a
///   daemon's trailing `*`) are stripped, but a leading `%` (bash job-control
///   syntax: `%1`/`%+`/`%-`/`%string`) and `@name` are preserved — the local
///   resolver still owns them.
/// - `stage` is the numeric stage of a `[N:M]*` / `N:M` target (a `N:@name`
///   stage stays in `core` for the attach resolver).
/// - `prefer_daemon` is set ONLY by the daemon marker `[N]*` / `[N:M]*`: it dis
///   ambiguates a local/daemon id collision toward the daemon job, so a starred
///   id never resolves to a local job 1 that happens to coexist. A plain `[N]`
///   is bash's LOCAL job display — not daemon — so it does not set the flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedJobTarget {
    pub core: String,
    pub stage: Option<u32>,
    pub prefer_daemon: bool,
}

/// Parse any rendered or typed job target. See [`ParsedJobTarget`]. Inverse of
/// [`render_job_id`] for the forms that function produces.
pub fn parse_job_target(input: &str) -> ParsedJobTarget {
    let t = input.trim();
    // Daemon form: a bracketed id with the trailing survival marker `[N]*` /
    // `[N:M]*`. Checked first — the `*` is what distinguishes it from bash's
    // local `[N]`.
    if let Some(inner) = t
        .strip_suffix('*')
        .and_then(|s| s.strip_prefix('['))
        .and_then(|s| s.strip_suffix(']'))
    {
        let (core, stage) = split_stage(inner);
        return ParsedJobTarget {
            core,
            stage,
            prefer_daemon: true,
        };
    }
    // Bash's LOCAL job display: `[N]` / `[N:M]` (no star). Strip the brackets;
    // resolution stays local-first.
    if let Some(inner) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        let (core, stage) = split_stage(inner);
        return ParsedJobTarget {
            core,
            stage,
            prefer_daemon: false,
        };
    }
    let (core, stage) = split_stage(t);
    ParsedJobTarget {
        core,
        stage,
        prefer_daemon: false,
    }
}

/// Split a `job:stage` target. Only a NUMERIC stage is captured; a `@name`
/// stage (the `N:@name` attach form) is left whole in `core` so the attach
/// resolver still sees it.
fn split_stage(s: &str) -> (String, Option<u32>) {
    if let Some((job, stage)) = s.split_once(':')
        && let Ok(n) = stage.parse::<u32>()
    {
        return (job.to_string(), Some(n));
    }
    (s.to_string(), None)
}

#[derive(Debug, Clone)]
pub enum JobKind {
    Shell { cmd: String },
    Agent { agent_id: Uuid, agent_name: String },
    ForgeApp { app_name: String },
}

impl fmt::Display for JobKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // For `Shell` jobs we display the command itself so
            // background spawn notifications read like bash's
            // `[1] PID cmd` — e.g. `[1] spawned: make build pid=...`.
            Self::Shell { cmd } => write!(f, "{cmd}"),
            Self::Agent { agent_name, .. } => write!(f, "agent:{agent_name}"),
            Self::ForgeApp { app_name } => write!(f, "app:{app_name}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum JobState {
    Foreground,
    Running,
    Stopped,
    Done { exit_code: i32 },
    Failed { reason: String },
}

impl fmt::Display for JobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Foreground => write!(f, "fg"),
            Self::Running => write!(f, "running"),
            Self::Stopped => write!(f, "stopped"),
            Self::Done { exit_code } => write!(f, "done({exit_code})"),
            Self::Failed { reason } => write!(f, "failed: {reason}"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum JobEvent {
    Spawned {
        id: JobId,
        kind: JobKind,
        pid: Option<u32>,
    },
    Attached {
        id: JobId,
    },
    Detached {
        id: JobId,
    },
    /// `label` carries the job's original command line so the
    /// renderer can format bash-compatible notifications like
    Stopped {
        id: JobId,
        label: String,
    },
    Continued {
        id: JobId,
        label: String,
    },
    Completed {
        id: JobId,
        exit_code: i32,
        label: String,
    },
}

impl JobEvent {
    pub fn job_id(&self) -> JobId {
        match self {
            Self::Spawned { id, .. }
            | Self::Attached { id }
            | Self::Detached { id }
            | Self::Stopped { id, .. }
            | Self::Continued { id, .. }
            | Self::Completed { id, .. } => *id,
        }
    }
}

#[derive(Debug, Clone)]
pub struct JobInfo {
    pub id: JobId,
    pub kind: JobKind,
    pub state: JobState,
    pub label: String,
    pub pid: Option<u32>,
    pub runtime: Duration,
    /// responses are piped into a downstream command. `None` otherwise.
    pub sink: Option<String>,
}

#[cfg(test)]
mod target_tests {
    use super::*;

    /// THE invariant: every id `render_job_id` produces must parse back to the
    /// same job (and stage). A display form that does not round-trip is a
    /// broken affordance — the user could not retype what `ps` shows.
    #[test]
    fn rendered_ids_round_trip() {
        let cases = [
            (JobOwner::Local, 1u32, None),
            (JobOwner::Local, 42, None),
            (JobOwner::Daemon, 1, None),
            (JobOwner::Daemon, 3, Some(2u32)),
        ];
        for (owner, id, stage) in cases {
            let rendered = render_job_id(owner, id, stage);
            let parsed = parse_job_target(&rendered);
            assert_eq!(
                parsed.core,
                id.to_string(),
                "core mismatch for {rendered}"
            );
            assert_eq!(parsed.stage, stage, "stage mismatch for {rendered}");
            assert_eq!(
                parsed.prefer_daemon,
                owner == JobOwner::Daemon,
                "prefer_daemon mismatch for {rendered}",
            );
        }
    }

    #[test]
    fn preserves_bash_and_at_forms() {
        // `%`-forms and `@name` are NOT decoration — the local resolver owns
        // them, so they survive verbatim in `core`.
        for raw in ["%1", "%+", "%-", "%make", "@faye", "faye", "1"] {
            let parsed = parse_job_target(raw);
            assert_eq!(parsed.core, raw, "{raw} must pass through untouched");
            assert_eq!(parsed.stage, None);
            assert!(!parsed.prefer_daemon);
        }
    }

    #[test]
    fn at_name_stage_is_not_split() {
        // `1:@stage` (attach form) keeps its `@name` stage in core — only a
        // numeric stage is captured.
        let parsed = parse_job_target("1:@sage");
        assert_eq!(parsed.core, "1:@sage");
        assert_eq!(parsed.stage, None);
    }
}

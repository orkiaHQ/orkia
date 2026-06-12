// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Top-level run loop: bring the session up, iterate the (currently
//! empty) flow registry, collect per-flow reports, fold into a single
//! [`CheckReport`].

use std::collections::BTreeMap;
use std::time::Instant;

use chrono::Utc;
use orkia_e2e_harness::{FlowEnv, HarnessError, OrkiaSession};

use crate::cli::Cli;
use crate::flows::{FlowDef, registry};
use crate::report::{
    CheckReport, FailureDetail, FlowReport, FlowStatus, InfraState, InfraStatus, ModeOut,
    RunStatus, Summary,
};

pub struct RunOutcome {
    pub report: CheckReport,
    pub exit_code: i32,
}

pub async fn run(cli: &Cli) -> RunOutcome {
    let started = Utc::now();
    let t0 = Instant::now();
    let mode_out = mode_out(cli);

    let flows = filter_flows(registry(), cli.filter.as_deref());

    // Filter that matched nothing → exit 3, regardless of mode.
    if cli.filter.is_some() && flows.is_empty() {
        return finalize_filter_miss(started, t0, mode_out);
    }

    match run_groups(cli, flows).await {
        Ok((flow_reports, failures)) => {
            finalize_ok(started, t0, mode_out, flow_reports, failures).await
        }
        Err(infra) => finalize_infra_error(started, t0, mode_out, infra),
    }
}

fn mode_out(cli: &Cli) -> ModeOut {
    match cli.mode {
        crate::cli::ModeArg::Local => ModeOut::Local,
        crate::cli::ModeArg::Compose => ModeOut::Compose,
    }
}

fn filter_flows(all: Vec<FlowDef>, filter: Option<&str>) -> Vec<FlowDef> {
    match filter {
        Some(pat) => all.into_iter().filter(|f| f.id.contains(pat)).collect(),
        None => all,
    }
}

/// Boot a session for a specific env (or fail with a descriptive infra
/// error). The harness's `start_compose_with_env` owns the health-wait
/// loop, so the runner just propagates whatever it returns. Local mode
/// (the B4 fallback) reuses the compose path and ignores the env's plan.
async fn boot_session(cli: &Cli, env: &FlowEnv) -> Result<OrkiaSession, InfraFailure> {
    let result = match cli.mode {
        crate::cli::ModeArg::Local => OrkiaSession::start_local().await,
        crate::cli::ModeArg::Compose => OrkiaSession::start_compose_with_env(env.clone()).await,
    };
    result.map_err(InfraFailure::from_harness)
}

/// Group flows by their required env, then boot one session per group
/// (Free sorts first via `FlowEnv`'s `Ord`) and run that group's flows
/// in it. Flows within a group preserve declaration order. Each flow's
/// `env_group` is stamped from the group it ran in.
///
/// The 17 (now 21) existing flows are all Free → a single group → a
/// single session, identical to the pre-multi-session behavior. Only the
/// SoloPro group (F403) triggers a second boot.
///
/// Between flows in a group we call [`OrkiaSession::reset_for_next_flow`];
/// each group's session is independent (no shared state across groups),
/// which is strictly safer than the old single-session model.
async fn run_groups(
    cli: &Cli,
    flows: Vec<FlowDef>,
) -> Result<(Vec<FlowReport>, Vec<FailureDetail>), InfraFailure> {
    let mut groups: BTreeMap<FlowEnv, Vec<FlowDef>> = BTreeMap::new();
    for f in flows {
        groups.entry(f.required_env.clone()).or_default().push(f);
    }

    let mut reports = Vec::new();
    let mut failures = Vec::new();

    for (env, group_flows) in groups {
        tracing::info!(plan = ?env.plan, count = group_flows.len(), "booting session for env group");
        let mut session = boot_session(cli, &env).await?;
        for (i, f) in group_flows.into_iter().enumerate() {
            if i > 0
                && let Err(e) = session.reset_for_next_flow().await
            {
                tracing::warn!("inter-flow reset failed: {e}");
            }
            let mut report = (f.run)(&mut session).await;
            report.env_group = env.label().to_string();
            if let Some(fail) = &report.failure {
                failures.push(fail.clone());
            }
            reports.push(report);
        }
    }

    Ok((reports, failures))
}

fn summarize(flows: &[FlowReport]) -> Summary {
    let mut s = Summary {
        total: flows.len(),
        passed: 0,
        failed: 0,
        skipped: 0,
        errored: 0,
    };
    for f in flows {
        match f.status {
            FlowStatus::Pass => s.passed += 1,
            FlowStatus::Fail => s.failed += 1,
            FlowStatus::Skipped => s.skipped += 1,
            FlowStatus::Errored => s.errored += 1,
        }
    }
    s
}

async fn finalize_ok(
    started: chrono::DateTime<Utc>,
    t0: Instant,
    mode_out: ModeOut,
    flows: Vec<FlowReport>,
    failures: Vec<FailureDetail>,
) -> RunOutcome {
    let summary = summarize(&flows);
    let status = if summary.failed > 0 || summary.errored > 0 {
        RunStatus::Fail
    } else {
        RunStatus::Pass
    };
    let exit_code = if status == RunStatus::Pass { 0 } else { 1 };
    let finished = Utc::now();
    RunOutcome {
        report: CheckReport {
            version: "1",
            started_at: started,
            finished_at: finished,
            duration_ms: elapsed_ms(t0),
            mode: mode_out,
            status,
            exit_code,
            summary,
            flows,
            failures,
            infrastructure: InfraStatus {
                docker_compose: match mode_out {
                    ModeOut::Compose => InfraState::Running,
                    ModeOut::Local => InfraState::Unknown,
                },
                backend_health: InfraState::Ok,
                // boot_session opened the PgPool — if it returned Ok,
                // postgres responded.
                postgres_health: InfraState::Ok,
            },
        },
        exit_code,
    }
}

fn finalize_filter_miss(
    started: chrono::DateTime<Utc>,
    t0: Instant,
    mode_out: ModeOut,
) -> RunOutcome {
    let finished = Utc::now();
    RunOutcome {
        report: CheckReport {
            version: "1",
            started_at: started,
            finished_at: finished,
            duration_ms: elapsed_ms(t0),
            mode: mode_out,
            status: RunStatus::Error,
            exit_code: 3,
            summary: zero_summary(),
            flows: Vec::new(),
            failures: Vec::new(),
            infrastructure: InfraStatus::all_unknown(),
        },
        exit_code: 3,
    }
}

fn finalize_infra_error(
    started: chrono::DateTime<Utc>,
    t0: Instant,
    mode_out: ModeOut,
    infra: InfraFailure,
) -> RunOutcome {
    let failure = FailureDetail {
        code: "INFRA_UNREACHABLE".to_string(),
        message: infra.message,
        expected: "backend reachable and harness boot succeeded".to_string(),
        actual: "boot failed before flows could run".to_string(),
        hypothesis: "Check docker-compose stack health, or wire start_local/start_compose."
            .to_string(),
        logs_at: String::new(),
        rendered_output_excerpt: String::new(),
        related_specs: vec!["e2e-harness".to_string()],
    };
    let finished = Utc::now();
    RunOutcome {
        report: CheckReport {
            version: "1",
            started_at: started,
            finished_at: finished,
            duration_ms: elapsed_ms(t0),
            mode: mode_out,
            status: RunStatus::Error,
            exit_code: 2,
            summary: zero_summary(),
            flows: Vec::new(),
            failures: vec![failure],
            infrastructure: InfraStatus {
                docker_compose: infra.compose,
                backend_health: infra.backend,
                postgres_health: infra.postgres,
            },
        },
        exit_code: 2,
    }
}

fn zero_summary() -> Summary {
    Summary {
        total: 0,
        passed: 0,
        failed: 0,
        skipped: 0,
        errored: 0,
    }
}

fn elapsed_ms(t0: Instant) -> u64 {
    u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX)
}

struct InfraFailure {
    message: String,
    backend: InfraState,
    postgres: InfraState,
    compose: InfraState,
}

impl InfraFailure {
    fn from_harness(err: HarnessError) -> Self {
        // `Infra(_)` from `start_compose` means the backend or postgres
        // did not come up in time; treat that as Unreachable, not Unknown.
        let (backend, postgres) = match &err {
            HarnessError::Infra(_) | HarnessError::Http(_) => {
                (InfraState::Unreachable, InfraState::Unknown)
            }
            HarnessError::Db(_) => (InfraState::Ok, InfraState::Unreachable),
            _ => (InfraState::Unknown, InfraState::Unknown),
        };
        Self {
            message: format!("harness boot failed: {err}"),
            backend,
            postgres,
            compose: InfraState::Unknown,
        }
    }
}

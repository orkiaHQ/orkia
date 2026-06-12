// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Shared flow helpers: report builders, diagnostics, login, and

use crate::report::{FailureDetail, FlowReport, FlowStatus};
use orkia_e2e_harness::{AssertKind, HarnessError, OrkiaSession};
use std::time::{Duration, Instant};

#[allow(clippy::too_many_arguments)] // failure builder with explicit related-specs arg
pub(crate) fn fail_with(
    id: &str,
    name: &str,
    t0: Instant,
    stages: &[String],
    stage: &str,
    code: &str,
    msg: String,
    hypothesis: &str,
    related_specs: &[String],
) -> FlowReport {
    let mut s = stages.to_vec();
    s.push(stage.into());
    FlowReport {
        id: id.into(),
        name: name.into(),
        status: FlowStatus::Fail,
        duration_ms: elapsed_ms(t0),
        env_group: String::new(),
        stages_completed: s,
        stage_failed: Some(stage.into()),
        failure: Some(FailureDetail {
            code: code.into(),
            message: msg,
            expected: String::new(),
            actual: String::new(),
            hypothesis: hypothesis.into(),
            logs_at: String::new(),
            rendered_output_excerpt: String::new(),
            related_specs: related_specs.to_vec(),
        }),
    }
}

#[allow(clippy::too_many_arguments)] // single-purpose failure builder; refactoring to a struct would obscure the call sites
pub(crate) fn fail_report(
    id: &str,
    name: &str,
    t0: Instant,
    stages: &[String],
    stage: &str,
    code: &str,
    msg: String,
    hypothesis: &str,
) -> FlowReport {
    let mut s = stages.to_vec();
    s.push(stage.into());
    FlowReport {
        id: id.into(),
        name: name.into(),
        status: FlowStatus::Fail,
        duration_ms: elapsed_ms(t0),
        env_group: String::new(),
        stages_completed: s,
        stage_failed: Some(stage.into()),
        failure: Some(FailureDetail {
            code: code.into(),
            message: msg,
            expected: String::new(),
            actual: String::new(),
            hypothesis: hypothesis.into(),
            logs_at: String::new(),
            rendered_output_excerpt: String::new(),
            related_specs: vec!["auth".into()],
        }),
    }
}

/// Read the most-recently-appended decision-id from
/// `<data_dir>/projects/<p>/decisions/<slug>.jsonl`. Returns the `did`
/// field of the last non-empty line.
pub(crate) fn find_decision_id(
    session: &OrkiaSession,
    project: &str,
    slug: &str,
) -> Result<String, String> {
    let shell = session.shell().ok_or("shell not booted")?;
    let path = shell
        .data_dir
        .join("projects")
        .join(project)
        .join("decisions")
        .join(format!("{slug}.jsonl"));
    let content =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let last = content
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .ok_or("decisions file is empty")?;
    let v: serde_json::Value =
        serde_json::from_str(last).map_err(|e| format!("parse jsonl: {e}"))?;
    v.get("id")
        .or_else(|| v.get("did"))
        .or_else(|| v.get("decision_id"))
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("no `did` field in: {last}"))
}

pub(crate) fn classify(err: &HarnessError) -> String {
    match err {
        HarnessError::Timeout(_) => "TIMEOUT".into(),
        HarnessError::Assertion { .. } => "ASSERTION_FAILED".into(),
        HarnessError::Infra(_) => "INFRA_UNREACHABLE".into(),
        _ => "RUNTIME_ERROR".into(),
    }
}

/// Build a fail-stage `FlowReport` from a `HarnessError`. Pulls the
/// assertion state-dump (if present) into `actual` and the PTY screen
/// (if applicable) into `rendered_output_excerpt`. All flow code
/// should route fails through here so diagnostic dumps stay uniform.
#[allow(clippy::too_many_arguments)] // failure builder with explicit related-specs + hypothesis
pub(crate) fn fail_with_diagnostics(
    id: &str,
    name: &str,
    t0: Instant,
    stages: &[String],
    stage: &str,
    err: &HarnessError,
    hypothesis: &str,
    related_specs: &[String],
    session: &OrkiaSession,
) -> FlowReport {
    let code = classify(err);
    let (actual, embed_kind) = match err {
        HarnessError::Assertion { state, kind, .. } => (state.clone(), Some(*kind)),
        HarnessError::Timeout(_) => {
            // Timeouts on PTY waits — include the last 3000 chars of
            // raw PTY (escape-decoded) so callers see what actually
            // appeared, even when alacritty's 24-line grid wrapped
            // earlier content off-screen.
            let raw_tail = session
                .shell()
                .map(|s| {
                    let raw = s.process.pty.raw_text();
                    let len = raw.len();
                    let start = len.saturating_sub(3000);
                    // `raw` is lossy-decoded PTY bytes; that byte offset may
                    // land mid-codepoint and panic — walk back to a char
                    // boundary (BUG-087).
                    let start = (0..=start)
                        .rev()
                        .find(|&i| raw.is_char_boundary(i))
                        .unwrap_or(0);
                    raw[start..].replace('\x1b', "\\e")
                })
                .unwrap_or_default();
            (
                format!("--- raw tail (last 3000 chars) ---\n{raw_tail}"),
                None,
            )
        }
        _ => (String::new(), None),
    };
    // PTY screen always useful when the failure is anchored on the
    // shell, AND when the harness error itself came from a PTY surface.
    let rendered = match embed_kind {
        Some(AssertKind::Output) => String::new(), // already in actual
        _ => session
            .shell()
            .map(|s| s.process.pty.screen_text())
            .unwrap_or_default(),
    };
    let mut s = stages.to_vec();
    s.push(stage.into());
    FlowReport {
        id: id.into(),
        name: name.into(),
        status: FlowStatus::Fail,
        duration_ms: elapsed_ms(t0),
        env_group: String::new(),
        stages_completed: s,
        stage_failed: Some(stage.into()),
        failure: Some(FailureDetail {
            code,
            message: format!("{err}"),
            expected: String::new(),
            actual,
            hypothesis: hypothesis.into(),
            logs_at: String::new(),
            rendered_output_excerpt: rendered,
            related_specs: related_specs.to_vec(),
        }),
    }
}

pub(crate) fn elapsed_ms(t0: Instant) -> u64 {
    u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Wait for the shell to reach its first prompt. The harness already
/// logged the per-plan fixture account in for real against the compose
/// backend (signed JWT persisted to the session file the shell reads at
/// boot), so there is no in-shell `login` to run — the interactive
/// magic-link flow can't complete headless. Flows that need to observe
/// the resolved plan assert it themselves via `plan`/`whoami`.
pub(crate) async fn boot_login(session: &mut OrkiaSession) -> Result<(), HarnessError> {
    if !session.has_shell() {
        return Err(HarnessError::Infra("shell not booted".into()));
    }
    // 20s, not 10s: a debug-build shell cold-starts slowly (first spawn
    // after a build, cold disk cache) and now does a real backend login at
    // boot before the first prompt. The early flows in a run hit this.
    session
        .wait_for("\x1b]133;A", Duration::from_secs(20))
        .await?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    Ok(())
}

pub(crate) fn pass_report(id: &str, name: &str, t0: Instant, stages: Vec<String>) -> FlowReport {
    FlowReport {
        id: id.into(),
        name: name.into(),
        status: FlowStatus::Pass,
        duration_ms: elapsed_ms(t0),
        env_group: String::new(),
        stages_completed: stages,
        stage_failed: None,
        failure: None,
    }
}

pub(crate) const S2_RELATED: &[&str] = &["seal", "rfc"];

/// Helper: scan `<data_dir>/<subdir>/` for a file whose name starts with
/// `<slug>-` and ends with `<suffix>`. Retries up to `timeout` since the
/// assembler may flush after the orkia command returns.
pub(crate) fn locate_seal_file(
    session: &OrkiaSession,
    subdir: &str,
    slug: &str,
    suffix: &str,
    timeout: Duration,
) -> Option<std::path::PathBuf> {
    let shell = session.shell()?;
    let dir = shell.data_dir.join(subdir);
    let prefix = format!("{slug}-");
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.starts_with(&prefix) && n.ends_with(suffix) {
                    return Some(e.path());
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Helper: complete RFC lifecycle (create → ask → resolve → promote → complete).
/// Used by F204 as prelude. Returns the slug on success.
pub(crate) async fn build_complete_rfc(
    session: &mut OrkiaSession,
    slug: &str,
    proj: &str,
) -> Result<(), HarnessError> {
    session
        .run(
            &format!("rfc create {slug} --title 'F204 prelude' {proj}"),
            slug,
            Duration::from_secs(10),
        )
        .await?;
    session
        .run(
            &format!("rfc cd {slug} {proj}"),
            slug,
            Duration::from_secs(5),
        )
        .await?;
    session
        .run(
            &format!("rfc ask {slug} --q anything --rationale e2e {proj}"),
            "clarification",
            Duration::from_secs(5),
        )
        .await?;
    let did = find_decision_id(session, "default-project", slug)
        .map_err(|e| HarnessError::Infra(format!("decision-id: {e}")))?;
    session
        .run(
            &format!("rfc resolve {did} {slug} --answer yes {proj}"),
            "resolved",
            Duration::from_secs(5),
        )
        .await?;
    session
        .run(
            &format!("rfc promote {slug} --yes {proj}"),
            "Active",
            Duration::from_secs(5),
        )
        .await?;
    session
        .run(
            &format!("rfc complete {slug} --yes {proj}"),
            "SEAL v1 document",
            Duration::from_secs(15),
        )
        .await?;
    Ok(())
}

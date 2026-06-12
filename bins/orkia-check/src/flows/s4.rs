// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! S4 auth/scope/capability flows (F401 team refusal, F402 scope hierarchy,
//! F403/F403b capability presence/absence).
//!
//! Audit findings baked in:
//!   * `team create` → NoopTeamClient → "…require Orkia Team. See https://orkia.dev/team …".
//!   * `config set default_scope <s>` → "✓ default_scope = <s>"; rejection →
//!     "illegal scope override: … child cannot be more permissive than parent".
//!   * `plan` renders lowercase describe() strings, not enum names.

use super::shared::*;
use crate::report::FlowReport;
use orkia_e2e_harness::{OrkiaSession, Plan};
use std::time::{Duration, Instant};

const S4_SCOPE_RELATED: &[&str] = &["scope", "rfc"];

/// F401 — `team create` is refused cleanly in OSS (NoopTeamClient), with
/// the upgrade message + URL and no shell disruption. Boundary test, like
/// F203 (forge) and F301 (pipeline).
pub(crate) async fn flow_f401(session: &mut OrkiaSession) -> FlowReport {
    let id = "F401-team-create-refusal";
    let name = "team create refused cleanly in OSS via NoopTeamClient";
    let t0 = Instant::now();
    let mut stages = Vec::<String>::new();
    let related: Vec<String> = ["team-shell", "shell"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    if let Err(e) = boot_login(session).await {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "boot_login",
            &e,
            "See F101.",
            &related,
            session,
        );
    }
    stages.push("boot_login".into());

    if let Err(e) = session
        .run(
            "team create my-team",
            "requires Orkia Team",
            Duration::from_secs(5),
        )
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "team_create_refused",
            &e,
            "If 'requires Orkia Team' missing: NoopTeamClient message changed (team.rs:392) or a real \
             team_client got wired into OSS — check repl.rs:271 still defaults to NoopTeamClient and \
             bins/orkia/src/main.rs does NOT call with_team_client(real). \
             If a team was actually created: critical boundary breach.",
            &related,
            session,
        );
    }
    stages.push("team_create_refused".into());

    if let Err(e) = session.output().contains("orkia.dev/team") {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "assert_upgrade_link",
            &e,
            "Refusal message must surface the upgrade URL 'orkia.dev/team' (team.rs:392).",
            &related,
            session,
        );
    }
    stages.push("assert_upgrade_link".into());

    if let Err(e) = session
        .run("whoami", Plan::Free.fixture_email(), Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "shell_still_responsive",
            &e,
            "If whoami fails after the refusal: the team builtin corrupted REPL state or panicked. \
             NoopTeamClient must return Err gracefully (team_builtins.rs err_block), not panic.",
            &related,
            session,
        );
    }
    stages.push("shell_still_responsive".into());

    pass_report(id, name, t0, stages)
}

/// F402 — scope-override validation (pure local, no team_client/backend).
/// A child artifact may not be more permissive than its parent. Two
/// directions: Public RFC under a Private default is rejected; Team RFC
/// under a Public default is allowed and persists `scope = "team"`.
pub(crate) async fn flow_f402(session: &mut OrkiaSession) -> FlowReport {
    let id = "F402-scope-validation-hierarchy";
    let name = "Scope hierarchy: child cannot be more permissive than workspace default";
    let t0 = Instant::now();
    let mut stages = Vec::<String>::new();
    let related: Vec<String> = S4_SCOPE_RELATED.iter().map(|s| s.to_string()).collect();
    let proj = "--project default-project";

    if let Err(e) = boot_login(session).await {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "boot_login",
            &e,
            "See F101.",
            &related,
            session,
        );
    }
    stages.push("boot_login".into());

    // ── Part 1: Private default rejects a Public RFC ──
    if let Err(e) = session
        .run(
            "config set default_scope private",
            "default_scope = private",
            Duration::from_secs(5),
        )
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "set_workspace_private",
            &e,
            "If the command fails: check the `config` builtin (config.rs). It must render \
             '✓ default_scope = private' and write config.toml in the data dir.",
            &related,
            session,
        );
    }
    stages.push("set_workspace_private".into());

    let leak_slug = "test-leak-doc";
    if let Err(e) = session
        .run(
            &format!("rfc create {leak_slug} --title 'leak attempt' --scope public {proj}"),
            "more permissive",
            Duration::from_secs(10),
        )
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "public_rfc_rejected_under_private",
            &e,
            "If a Public RFC is accepted under a Private default: scope hierarchy is not enforced on \
             the create path. validate_artifact_scope (scope_validation.rs:27) must reject child>parent, \
             and repl.rs:2982 must call it BEFORE rfc::create. Hierarchy: Private < Team < Public.",
            &related,
            session,
        );
    }
    // NOTE: we deliberately do NOT assert the slug is absent from the
    // rendered output — the typed command line echoes it (S1 retro: the
    // F101 not_contains("faye") false-positive). Proof of rejection is the
    // "more permissive" marker above + the on-disk file-absence check below.
    if let Err(e) = session.output().contains("illegal scope override") {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "public_rfc_rejected_under_private",
            &e,
            "Rejection should render the validator's 'illegal scope override' error (scope.rs:75). \
             If 'more permissive' matched but this didn't, the message wording diverged.",
            &related,
            session,
        );
    }
    // The rejected RFC must not exist on disk (validate runs before create).
    if let Some(shell) = session.shell() {
        let leak_path = shell
            .data_dir
            .join("projects")
            .join("default-project")
            .join("rfcs")
            .join(format!("{leak_slug}.md"));
        if leak_path.exists() {
            return fail_with(
                id,
                name,
                t0,
                &stages,
                "public_rfc_rejected_under_private",
                "ASSERTION_FAILED",
                format!("rejected RFC was persisted at {}", leak_path.display()),
                "validate_artifact_scope must run BEFORE rfc::create (repl.rs:2982 → 2993); the file \
                 must never be written when validation fails.",
                &related,
            );
        }
    }
    stages.push("public_rfc_rejected_under_private".into());

    // ── Part 2: Public default allows a Team RFC ──
    if let Err(e) = session
        .run(
            "config set default_scope public",
            "default_scope = public",
            Duration::from_secs(5),
        )
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "set_workspace_public",
            &e,
            "config set default_scope public must render '✓ default_scope = public'.",
            &related,
            session,
        );
    }
    stages.push("set_workspace_public".into());

    let ok_slug = "test-visible-doc";
    if let Err(e) = session
        .run(
            &format!("rfc create {ok_slug} --title 'visible doc' --scope team {proj}"),
            ok_slug,
            Duration::from_secs(10),
        )
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "team_rfc_allowed_under_public",
            &e,
            "If a Team RFC is rejected under a Public default: validation is over-eager or the \
             comparison direction is inverted. Team (less permissive) under Public (more permissive) \
             must pass: child <= parent. Check validate_override (scope.rs).",
            &related,
            session,
        );
    }
    stages.push("team_rfc_allowed_under_public".into());

    // ── Part 3: the team RFC persisted with scope = "team" in frontmatter ──
    let Some(shell) = session.shell() else {
        return fail_with(
            id,
            name,
            t0,
            &stages,
            "persisted_scope_is_team",
            "INFRA_UNREACHABLE",
            "shell not booted".into(),
            "Shell vanished mid-flow.",
            &related,
        );
    };
    let ok_path = shell
        .data_dir
        .join("projects")
        .join("default-project")
        .join("rfcs")
        .join(format!("{ok_slug}.md"));
    match std::fs::read_to_string(&ok_path) {
        Ok(body) if body.contains("scope = \"team\"") => {}
        Ok(body) => {
            return fail_with(
                id,
                name,
                t0,
                &stages,
                "persisted_scope_is_team",
                "ASSERTION_FAILED",
                format!("RFC created but frontmatter lacks `scope = \"team\"`:\n{body}"),
                "rfc::create tags scope via Workspace::update_rfc(.., \"scope\", \"team\") (rfc.rs). \
                 If the tag is missing, the --scope flag didn't round-trip to the frontmatter.",
                &related,
            );
        }
        Err(e) => {
            return fail_with(
                id,
                name,
                t0,
                &stages,
                "persisted_scope_is_team",
                "ASSERTION_FAILED",
                format!("team RFC not persisted at {}: {e}", ok_path.display()),
                "create reported success but no file on disk — check rfc::create write path.",
                &related,
            );
        }
    }
    stages.push("persisted_scope_is_team".into());

    pass_report(id, name, t0, stages)
}

/// F403 — logged in as the SoloPro fixture (`solo@e2e.orkia.dev`, its own
/// session group; plan resolved from the backend), the `plan` builtin
/// shows the unlocked cognitive capabilities. Forms a distinguishing pair
/// with F403b (free).
pub(crate) async fn flow_f403(session: &mut OrkiaSession) -> FlowReport {
    let id = "F403-capability-presence-solo-pro";
    let name = "solo-pro plan unlocks cognitive routing + context compression";
    let t0 = Instant::now();
    let mut stages = Vec::<String>::new();
    let related: Vec<String> = ["capabilities"].iter().map(|s| s.to_string()).collect();

    if let Err(e) = boot_login(session).await {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "boot_login",
            &e,
            "See F101.",
            &related,
            session,
        );
    }
    stages.push("boot_login".into());

    // `plan` renders the header then the unlocked-capability list. Wait
    // for a capability string (proves the list rendered, not just the header).
    if let Err(e) = session
        .run("plan", "cognitive routing", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "capabilities_unlocked",
            &e,
            "If no capabilities under solo-pro: the session's plan isn't solo-pro. \
             Check this flow's required_env is Plan::SoloPro (so the harness logs in as \
             solo@e2e.orkia.dev) and that the boot-time login succeeded (login::login_to_session_file). \
             If header shows 'free': the backend resolved the wrong plan for that fixture, or the \
             session file wasn't loaded. capabilities_for_plan(SoloPro) must include \
             CognitiveRouting/ContextCompression/CognitiveRouter/ForgeBuild (plan.rs:49).",
            &related,
            session,
        );
    }
    stages.push("capabilities_unlocked".into());

    if let Err(e) = session.output().contains("solo-pro") {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "plan_shows_solo_pro",
            &e,
            "plan header should read 'plan: solo-pro (account @…)' (auth_builtins.rs:164). \
             If it shows 'free', the backend resolved the wrong plan for solo@e2e.orkia.dev \
             (check the seed's billing_plan) or this group's session file wasn't loaded.",
            &related,
            session,
        );
    }
    stages.push("plan_shows_solo_pro".into());

    if let Err(e) = session.output().contains("context compression") {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "context_compression_present",
            &e,
            "context compression missing but cognitive routing present → partial mapping. \
             Check the full SoloPro set in capabilities_for_plan (plan.rs:49).",
            &related,
            session,
        );
    }
    stages.push("context_compression_present".into());

    pass_report(id, name, t0, stages)
}

/// F403b — under the default `free` plan, the `plan` builtin shows no
/// premium capabilities. Distinguishing pair with F403: same builtin,
/// opposite expectation. Runs in the Free group (no extra boot).
pub(crate) async fn flow_f403b(session: &mut OrkiaSession) -> FlowReport {
    let id = "F403b-capability-absence-free";
    let name = "free plan has no premium capabilities (distinguishing pair with F403)";
    let t0 = Instant::now();
    let mut stages = Vec::<String>::new();
    let related: Vec<String> = ["capabilities"].iter().map(|s| s.to_string()).collect();

    if let Err(e) = boot_login(session).await {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "boot_login",
            &e,
            "See F101.",
            &related,
            session,
        );
    }
    stages.push("boot_login".into());

    if let Err(e) = session
        .run("plan", "no premium capabilities", Duration::from_secs(5))
        .await
    {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "plan_free_no_caps",
            &e,
            "Free plan must render 'no premium capabilities — run `login` to unlock cognitive features' \
             (auth_builtins.rs:171). If this marker is missing, either the session isn't Free \
             (check required_env defaults to Free) or capabilities_for_plan(Free) returned non-empty.",
            &related,
            session,
        );
    }
    stages.push("plan_free_no_caps".into());

    // The critical distinguishing assertion: premium cap strings must be
    // absent under free. If present, the gate leaks premium features.
    if let Err(e) = session.output().not_contains("cognitive routing") {
        return fail_with_diagnostics(
            id,
            name,
            t0,
            &stages,
            "no_premium_caps_leak",
            &e,
            "CRITICAL: 'cognitive routing' present under free → capabilities_for_plan(Free) is not \
             empty (plan.rs). The capability gate is leaking premium features to free users.",
            &related,
            session,
        );
    }
    stages.push("no_premium_caps_leak".into());

    pass_report(id, name, t0, stages)
}

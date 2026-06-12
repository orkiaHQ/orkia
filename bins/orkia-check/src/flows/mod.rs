// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

mod s0;
mod s1;
mod s2;
mod s3;
mod s4;
mod s5;
mod shared;

// can reference them by bare name.
use s0::*;
use s1::*;
use s2::*;
use s3::*;
use s4::*;
use s5::*;

use crate::report::FlowReport;
use orkia_e2e_harness::{FlowEnv, OrkiaSession, Plan};
use std::pin::Pin;

pub type FlowFn =
    fn(&mut OrkiaSession) -> Pin<Box<dyn std::future::Future<Output = FlowReport> + '_>>;

pub struct FlowDef {
    pub id: &'static str,
    pub name: &'static str,
    pub run: FlowFn,
    /// Environment this flow needs. The runner groups by this and boots
    /// one session per distinct value.
    pub required_env: FlowEnv,
}

impl FlowDef {
    /// A flow that runs in the default Free environment.
    fn free(id: &'static str, name: &'static str, run: FlowFn) -> Self {
        Self {
            id,
            name,
            run,
            required_env: FlowEnv::free(),
        }
    }

    /// A flow that requires a specific plan (boots its own session group).
    fn with_plan(id: &'static str, name: &'static str, run: FlowFn, plan: Plan) -> Self {
        Self {
            id,
            name,
            run,
            required_env: FlowEnv::with_plan(plan),
        }
    }

    /// A flow that requires extra env vars (own session group).
    fn with_env(id: &'static str, name: &'static str, run: FlowFn, env: FlowEnv) -> Self {
        Self {
            id,
            name,
            run,
            required_env: env,
        }
    }
}

pub fn registry() -> Vec<FlowDef> {
    vec![
        // ── S0–S3: all Free (one session group, behavior unchanged) ──
        FlowDef::free(
            "F001-boot-and-ps",
            "Boot shell and run ps with no agents",
            |s| Box::pin(flow_f001(s)),
        ),
        FlowDef::free(
            "F002-login-and-whoami",
            "Real backend session loaded at boot; whoami shows identity",
            |s| Box::pin(flow_f002(s)),
        ),
        FlowDef::free(
            "F003-rfc-complete-produces-seal-v1",
            "Create RFC, promote, complete; verify SEAL v1 document",
            |s| Box::pin(flow_f003(s)),
        ),
        FlowDef::free(
            "F004-shell-pipe-to-agent",
            "Pipe shell stdout into an agent via the | @faye syntax",
            |s| Box::pin(flow_f004(s)),
        ),
        FlowDef::free(
            "F005-agent-job-control",
            "Spawn / ps / attach / detach / kill an agent",
            |s| Box::pin(flow_f005(s)),
        ),
        FlowDef::free(
            "F101-multi-agent-ps",
            "Spawn faye + sage; ps shows both; kill each in turn",
            |s| Box::pin(flow_f101(s)),
        ),
        FlowDef::free(
            "F102-fg-bg-cycle",
            "Spawn agent, suspend via Ctrl-Z, bg, fg, detach, kill",
            |s| Box::pin(flow_f102(s)),
        ),
        FlowDef::free(
            "F103-wait-and-disown",
            "wait blocks until job done; disown detaches without killing",
            |s| Box::pin(flow_f103(s)),
        ),
        FlowDef::free(
            "F104-natural-completion",
            "Agent that exits naturally produces lifecycle:completed exit_code=0",
            |s| Box::pin(flow_f104(s)),
        ),
        FlowDef::free(
            "F105-crash-recovery",
            "Agent that aborts (SIGABRT) is detected via non-zero exit_code; next spawn works",
            |s| Box::pin(flow_f105(s)),
        ),
        FlowDef::free(
            "F106-tui-cockpit-daemon-contract",
            "TUI cockpit daemon contract exposes ps/status/inspect/logs/tell/stop/wait",
            |s| Box::pin(flow_f106(s)),
        ),
        FlowDef::free(
            "F201-rfc-ask-resolve",
            "Create RFC, ask clarification, resolve, verify state transitions and journal events",
            |s| Box::pin(flow_f201(s)),
        ),
        FlowDef::free(
            "F202-rfc-abandon-seal-v1",
            "Abandon RFC → SEAL v1 document with `abandoned` closure",
            |s| Box::pin(flow_f202(s)),
        ),
        FlowDef::free(
            "F203-rfc-forge-noop-oss",
            "rfc forge on OSS binary returns premium-required error gracefully",
            |s| Box::pin(flow_f203(s)),
        ),
        FlowDef::free(
            "F204-seal-v1-tampering",
            "Modify one byte in a SEAL v1 document, verify fails",
            |s| Box::pin(flow_f204(s)),
        ),
        FlowDef::free(
            "F205-seal-v1-multi-event",
            "Produce SEAL v1 document with 20 events, ordered, verified",
            |s| Box::pin(flow_f205(s)),
        ),
        FlowDef::free(
            "F206-seal-v1-value-tampering",
            "Decrement events_count in footer; verify detects via signature failure",
            |s| Box::pin(flow_f206(s)),
        ),
        FlowDef::free(
            "F301-pipeline-oss-refuse",
            "Agent-to-agent pipeline refused cleanly in OSS, no spawn",
            |s| Box::pin(flow_f301(s)),
        ),
        // ── S4 Free group ──
        FlowDef::free(
            "F401-team-create-refusal",
            "team create refused cleanly in OSS via NoopTeamClient",
            |s| Box::pin(flow_f401(s)),
        ),
        FlowDef::free(
            "F402-scope-validation-hierarchy",
            "Scope hierarchy: child cannot be more permissive than workspace default",
            |s| Box::pin(flow_f402(s)),
        ),
        FlowDef::free(
            "F403b-capability-absence-free",
            "free plan has no premium capabilities (distinguishing pair with F403)",
            |s| Box::pin(flow_f403b(s)),
        ),
        // ── S4 SoloPro group (triggers a second session boot) ──
        FlowDef::with_plan(
            "F403-capability-presence-solo-pro",
            "solo-pro plan unlocks cognitive routing + context compression",
            |s| Box::pin(flow_f403(s)),
            Plan::SoloPro,
        ),
        // ── S5 Free group (scheduling CRUD + apps + contribute boundary) ──
        FlowDef::free(
            "F501-every-crud-roundtrip",
            "every create/list/pause/resume/remove against isolated crontab spool",
            |s| Box::pin(flow_f501(s)),
        ),
        FlowDef::free(
            "F503-app-inspect-perms",
            "app inspect and perms render seeded manifest fields",
            |s| Box::pin(flow_f503(s)),
        ),
        FlowDef::free(
            "F504-app-seal-verify-tamper",
            "Forge app provenance chain (ledger #3): tamper detection",
            |s| Box::pin(flow_f504(s)),
        ),
        FlowDef::free(
            "F505-app-usage-refusal",
            "app usage refused cleanly in OSS (premium-gated)",
            |s| Box::pin(flow_f505(s)),
        ),
        FlowDef::free(
            "F506-app-run-viewer-absent",
            "app run fails cleanly when orkia-forge-viewer is absent",
            |s| Box::pin(flow_f506(s)),
        ),
        FlowDef::free(
            "F507-contribute-kernel-absent",
            "contribute refused cleanly when kernel daemon absent",
            |s| Box::pin(flow_f507(s)),
        ),
        // ── S5 Scheduled group (ORKIA_SCHEDULED=1 → third session boot) ──
        FlowDef::with_env(
            "F502-every-scheduled-fire",
            "ORKIA_SCHEDULED invocation behaves as a crond fire (spawn or park)",
            |s| Box::pin(flow_f502(s)),
            FlowEnv::with_env(Plan::Free, vec![("ORKIA_SCHEDULED".into(), "1".into())]),
        ),
    ]
}

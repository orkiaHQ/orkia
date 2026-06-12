// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Pre-baked agent scripts for S1 job-control flows.
//!
//! S0 wrote a YAML string literal directly; S1 has 4 distinct scripts so
//! we build them from the typed `AgentScript` struct. Each builder takes
//! the agent name and embeds it in the greeting line so the flow can
//! `wait_for("<name> ready")` deterministically.
//!
//! Scripts:
//!   * `keepalive_script(name)` — greets, OSC 133, then awaits input until
//!     killed. Used by multi-agent ps, fg/bg cycle, and crash-recovery
//!     "spawn-next-agent" stage.
//!   * `natural_exit_script(name)` — greets, sleeps 2s, exits 0. Tests
//!     that natural exit produces `lifecycle:completed` with `exit_code: 0`.
//!   * `long_work_script(name)` — same shape but 5s sleep. Used by
//!     F103's `wait blocks ~5s` measurement.
//!   * `crash_script(name)` — greets then aborts (SIGABRT → exit code
//!     134). Tests that the reap path turns WIFSIGNALED into a
//!     `Completed` envelope with non-zero `exit_code`.

use orkia_test_harness::script::{AgentScript, CrashMode, Osc133Marker, ScriptStep};

const READY_TIMEOUT_MS: u64 = 600_000;

pub fn keepalive_script(name: &str) -> AgentScript {
    AgentScript {
        name: Some(format!("{name}-keepalive")),
        raw_mode: false,
        steps: vec![
            ScriptStep::Print {
                text: format!("{name} ready\n"),
            },
            ScriptStep::Osc133 {
                marker: Osc133Marker::PromptStart,
                exit_code: None,
            },
            ScriptStep::AwaitInput {
                bytes: None,
                until: None,
                timeout_ms: READY_TIMEOUT_MS,
            },
        ],
    }
}

pub fn natural_exit_script(name: &str) -> AgentScript {
    AgentScript {
        name: Some(format!("{name}-natural-exit")),
        raw_mode: false,
        steps: vec![
            ScriptStep::Print {
                text: format!("{name} starting work\n"),
            },
            ScriptStep::Sleep { ms: 2000 },
            ScriptStep::Print {
                text: format!("{name} done\n"),
            },
            ScriptStep::Exit { code: 0 },
        ],
    }
}

pub fn long_work_script(name: &str) -> AgentScript {
    AgentScript {
        name: Some(format!("{name}-long-work")),
        raw_mode: false,
        steps: vec![
            ScriptStep::Print {
                text: format!("{name} long work starting\n"),
            },
            ScriptStep::Sleep { ms: 5000 },
            ScriptStep::Print {
                text: format!("{name} long work done\n"),
            },
            ScriptStep::Exit { code: 0 },
        ],
    }
}

pub fn crash_script(name: &str) -> AgentScript {
    AgentScript {
        name: Some(format!("{name}-crash")),
        raw_mode: false,
        steps: vec![
            ScriptStep::Print {
                text: format!("{name} about to crash\n"),
            },
            // Abort produces a stable SIGABRT → exit code 134 on Unix.
            // Picked over SIGSEGV because it doesn't require unsafe and
            // is portable across hosts.
            ScriptStep::Crash {
                mode: CrashMode::Abort,
            },
        ],
    }
}

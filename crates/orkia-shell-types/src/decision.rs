// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Agent(String),
    Builtin,
    Shell,
    Contextual,
}

#[derive(Debug, Clone)]
pub enum Decision {
    Shell(String),
    /// A control/effect builtin: raw command `name` + `args`. Dispatched by
    /// name in `orkia-shell` (`Repl::dispatch_named`), where the per-command
    /// former `Builtin(BuiltinCmd)` (the `enum BuiltinCmd` + its two giant match
    /// blocks are gone).
    Builtin {
        name: String,
        args: Vec<String>,
    },
    /// A typed/streamed pipeline routed through the `CommandRegistry`
    /// `Repl::dispatch_exec`.
    Exec(crate::exec::ExecPlan),
    /// A plain agent dispatch (`@<agent> [body] [--once]`). Persistent by default;
    /// `--once` runs exactly one turn, prints the final response to the terminal,
    /// then kills the session.
    Agent {
        name: Option<String>,
        body: String,
        once: bool,
    },
    Pipeline(Vec<PipelineStage>),
    /// The shell prefix executes via brush-equivalent (`sh -c`), its
    /// captured stdout is appended to `body` and injected into the
    /// freshly-spawned agent via `StdinSource::InitialBytes`. Always
    /// a fresh agent job — the live-session reuse path is skipped.
    ShellToAgent {
        shell: String,
        agent: String,
        body: String,
    },
    /// Binds the agent session's per-turn response text to a downstream shell
    /// command (run via `sh -c` per completed turn, fed the text on stdin).
    /// Persistent by default — the live session is reused and `tell` feeds the
    /// same sink; `once` runs a single turn then kills the session.
    AgentToSink {
        agent: String,
        body: String,
        once: bool,
        sink_cmd: String,
    },
    NoOp(NoOpReason),
}

#[derive(Debug, Clone)]
pub struct PipelineStage {
    pub agent: String,
    pub body: String,
}

#[derive(Debug, Clone)]
pub enum NoOpReason {
    Empty,
    WhitespaceOnly,
}

#[derive(Debug, Clone)]
pub enum Outcome {
    ShellComplete {
        exit_code: i32,
        output: String,
    },
    BuiltinOutput {
        blocks: Vec<BlockContent>,
    },
    AgentStarted {
        agent: String,
        job_id: String,
    },
    JobSpawned {
        job_id: super::job::JobId,
        foreground: bool,
        /// REPL-local (`%N`, dies with the shell) vs daemon-owned (`[N]`,
        /// survives). Drives the spawn-ack prefix so the survival contract is
        /// legible at a glance.
        owner: super::job::JobOwner,
    },
    PipelineStarted {
        stages: Vec<String>,
    },
    Noop,
    Error(String),
    /// The command never ran because the *invocation* was malformed —
    /// unknown flag, bad arity, refused operator, gated target form.
    /// Renders identically to [`Outcome::Error`]; differs only in exit
    UsageError(String),
}

impl Outcome {
    /// One function, no scattered literals: success → 0, runtime failure
    /// → 1, usage error → 2. Shell commands report their own code
    /// (clamped into `u8`; negative/oversized codes become 1, matching
    /// "failed but code unrepresentable"). The reserved POSIX codes
    /// (124 timeout, 126 not-executable, 127 not-found) are never
    /// produced here — only real process execution can yield them.
    pub fn exit_code(&self) -> u8 {
        match self {
            Outcome::ShellComplete { exit_code, .. } => u8::try_from(*exit_code).unwrap_or(1),
            Outcome::BuiltinOutput { .. }
            | Outcome::AgentStarted { .. }
            | Outcome::JobSpawned { .. }
            | Outcome::PipelineStarted { .. }
            | Outcome::Noop => 0,
            Outcome::Error(_) => 1,
            Outcome::UsageError(_) => 2,
        }
    }
}

#[derive(Debug, Clone)]
pub enum BlockContent {
    Text(String),
    AgentMessage {
        agent: String,
        text: String,
    },
    ToolCall {
        agent: String,
        tool: String,
        target: String,
        duration_ms: u64,
        status: String,
    },
    Approval {
        agent: String,
        action: String,
        risk: String,
    },
    Attention {
        rows: Vec<crate::AttentionRow>,
        message: Option<String>,
    },
    SealRecord {
        seq: u64,
        agent: String,
        event: String,
        hash_short: String,
    },
    /// One pre-padded, per-cell-styled table row. The column header is emitted
    /// separately as a `SystemInfo` block (so the streaming sink's
    /// header-dedup still works). Each cell carries a `CellStyle` *hint* — no
    /// ANSI — so the structured `Value` behind the table stays colour-free for
    /// the pipeline (`where`/`sort_by`). Each renderer maps the hint to its own
    /// palette (ANSI in shell-mode, theme in TUI).
    TableRow(Vec<StyledCell>),
    /// A meta/diagnostic line with a severity hint — the coloured counterpart
    /// to `SystemInfo` (which is always dim). `Good` for a success
    /// confirmation, `Warn` for a soft refusal, `Dim`/`Plain` for neutral info.
    Notice {
        style: CellStyle,
        text: String,
    },
    SystemInfo(String),
    Error(String),
}

/// A single rendered table cell: text already padded to its column width, plus
/// a presentation hint. Carries no colour codes — the renderer resolves them.
#[derive(Debug, Clone)]
pub struct StyledCell {
    pub text: String,
    pub style: CellStyle,
}

/// Presentation hint for a table cell, resolved to a concrete colour by each
/// renderer. Deliberately semantic (Good/Warn/Bad), not literal (green/red),
/// so shell-mode ANSI and the TUI theme stay the single owners of the palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellStyle {
    Plain,
    Dim,
    Good,
    Warn,
    Bad,
    Accent,
}

impl CellStyle {
    /// Colour hint for an approval risk label. Free-text on the wire
    /// (`low`/`medium`/`high`/`critical`/`unknown`/…), so this stays a
    /// presentation-time mapping — the `risk` field itself is never re-typed.
    pub fn for_risk(risk: &str) -> Self {
        match risk.trim().to_ascii_lowercase().as_str() {
            "high" | "critical" | "severe" => CellStyle::Bad,
            "low" | "none" => CellStyle::Good,
            // medium / moderate / unknown / unrecognised → caution by default.
            _ => CellStyle::Warn,
        }
    }
}

/// Status of an approval request. Used for display and SEAL records.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
}

#[cfg(test)]
mod exit_code_tests {
    use super::Outcome;

    #[test]
    fn success_outcomes_report_zero() {
        for outcome in [
            Outcome::BuiltinOutput { blocks: vec![] },
            Outcome::AgentStarted {
                agent: "faye".into(),
                job_id: "1".into(),
            },
            Outcome::JobSpawned {
                job_id: crate::job::JobId(1),
                foreground: false,
                owner: crate::job::JobOwner::Local,
            },
            Outcome::PipelineStarted { stages: vec![] },
            Outcome::Noop,
        ] {
            assert_eq!(outcome.exit_code(), 0, "{outcome:?}");
        }
    }

    #[test]
    fn error_is_one_usage_error_is_two() {
        assert_eq!(Outcome::Error("boom".into()).exit_code(), 1);
        assert_eq!(Outcome::UsageError("usage: …".into()).exit_code(), 2);
    }

    #[test]
    fn shell_complete_passes_its_code_through() {
        for code in [0_i32, 1, 2, 7, 130, 255] {
            let outcome = Outcome::ShellComplete {
                exit_code: code,
                output: String::new(),
            };
            assert_eq!(i32::from(outcome.exit_code()), code);
        }
    }

    #[test]
    fn unrepresentable_shell_codes_clamp_to_one() {
        for code in [-1_i32, -130, 256, 1000] {
            let outcome = Outcome::ShellComplete {
                exit_code: code,
                output: String::new(),
            };
            assert_eq!(outcome.exit_code(), 1, "code {code} must clamp to 1");
        }
    }

    /// no non-shell outcome may ever map onto them.
    #[test]
    fn reserved_codes_never_come_from_non_shell_outcomes() {
        for outcome in [
            Outcome::BuiltinOutput { blocks: vec![] },
            Outcome::Noop,
            Outcome::Error("x".into()),
            Outcome::UsageError("x".into()),
        ] {
            assert!(
                ![124, 126, 127].contains(&outcome.exit_code()),
                "{outcome:?}"
            );
        }
    }
}

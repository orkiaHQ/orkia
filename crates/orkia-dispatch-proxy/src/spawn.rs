// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file for terms.

//! How a task crosses into a detached agent job (`SPEC-ORKIA-RFC-DISPATCH`
//! §3.2). The composed body is multi-line, so it can't ride in the command
//! line the way `rfc delegate` does. Instead the proxy writes it into the
//! task's issue ([`crate::issues`]) before spawning, and the spawned line is a
//! re-parseable `orkia rfc dispatch-task …` that names the RFC and task id;
//! the daemon's re-run handler (the command surface, step 6) reads the prompt
//! back out of `<rfc_dir>/issues/<id>.md` and injects it via the
//! detector-gated `pending_body` path.
//!
//! Both halves — the proxy that writes the issue, and the re-run handler that
//! reads it — share this helper so the command grammar has a single source of
//! truth.

/// The REPL command line the detached runtime re-parses to re-run one task
/// in-process. Sibling of the `orkia rfc delegate …` line: the runtime reads
/// the prompt from the task's issue, resolves `agent`, and dispatches with the
/// composed body as `pending_body`. `--project` pins the already-resolved
/// scope. The issue is keyed by `(rfc, task)`, so no run id is needed on the
/// line.
pub fn task_command_line(rfc_id: &str, task_id: &str, agent: &str, project: &str) -> String {
    format!(
        "orkia rfc dispatch-task {} --task {} --agent {} --project {}",
        quote_arg(rfc_id),
        quote_arg(task_id),
        quote_arg(agent),
        quote_arg(project),
    )
}

/// Single-quote an argument for the re-parsed command line iff it contains a
/// shell metacharacter. Kept local so this crate stays free of the shell's
/// quoting module (the re-run handler uses the same rule).
fn quote_arg(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_is_reparseable_grammar() {
        let line = task_command_line("ship-x", "t-api", "faye", "orkia");
        assert_eq!(
            line,
            "orkia rfc dispatch-task ship-x --task t-api --agent faye --project orkia"
        );
    }

    #[test]
    fn command_line_quotes_metachars() {
        let line = task_command_line("ship x", "t-api", "faye", "orkia");
        assert!(line.contains("'ship x'"), "spaces must be quoted: {line}");
    }
}

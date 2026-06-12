// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use orkia_shell::classifier::{HeuristicClassifier, IntentClassifier, IntentGuess, resolve_mode};
use orkia_shell::decision::Mode;

#[test]
fn resolves_agent_prefix() {
    assert_eq!(
        resolve_mode("@faye fix the bug"),
        Mode::Agent("faye".into())
    );
}

#[test]
fn resolves_builtin_via_orkia() {
    assert_eq!(resolve_mode("orkia ps"), Mode::Builtin);
    assert_eq!(resolve_mode("orkia"), Mode::Builtin);
    assert_eq!(resolve_mode("orkia route"), Mode::Builtin);
    assert_eq!(resolve_mode("orkia login"), Mode::Builtin);
    assert_eq!(resolve_mode("orkia log"), Mode::Builtin);
}

/// `orkia --flag …` is the binary's own CLI syntax (`orkia --detach -c '@a …'`,
/// `orkia --version`), not a builtin-namespace claim — it must reach brush,
/// which execs the real binary.
#[test]
fn orkia_flag_head_is_shell() {
    assert_eq!(
        resolve_mode("orkia --detach -c '@sage --once review src/auth.rs'"),
        Mode::Shell
    );
    assert_eq!(resolve_mode("orkia --version"), Mode::Shell);
    assert_eq!(resolve_mode("orkia -c 'orkia ps'"), Mode::Shell);
}

/// `orkia <cli-only-verb> …` is a subcommand of the binary with no REPL
/// dispatch arm (`logs`, `update`, `daemon`, …) — it must reach brush,
/// which execs the real binary. Genuine unknowns still error in-process.
#[test]
fn orkia_cli_only_verbs_are_shell() {
    assert_eq!(resolve_mode("orkia logs 1 --last 8"), Mode::Shell);
    assert_eq!(resolve_mode("orkia update --check"), Mode::Shell);
    assert_eq!(resolve_mode("orkia daemon status"), Mode::Shell);
    assert_eq!(resolve_mode("orkia inspect 2"), Mode::Shell);
    assert_eq!(resolve_mode("orkia nosuchcmd"), Mode::Builtin);
    // Bare CLI-only verbs without the namespace stay Contextual — the
    // bridge is namespace-gated, never a bare-word grab.
    assert_eq!(resolve_mode("logs 1"), Mode::Contextual);
    assert_eq!(resolve_mode("update"), Mode::Contextual);
}

#[test]
fn resolves_builtin_via_slash() {
    assert_eq!(resolve_mode("/audit"), Mode::Builtin);
    assert_eq!(resolve_mode("/ps"), Mode::Builtin);
}

#[test]
fn resolves_augmented_builtin_bare() {
    assert_eq!(resolve_mode("ps"), Mode::Builtin);
    assert_eq!(resolve_mode("ps -a"), Mode::Builtin);
    assert_eq!(resolve_mode("kill 1"), Mode::Builtin);
    assert_eq!(resolve_mode("fg 1"), Mode::Builtin);
    assert_eq!(resolve_mode("bg"), Mode::Builtin);
    assert_eq!(resolve_mode("history --agents"), Mode::Builtin);
    assert_eq!(resolve_mode("help"), Mode::Builtin);
}

#[test]
fn resolves_agentic_builtin_bare() {
    assert_eq!(resolve_mode("attach @faye"), Mode::Builtin);
    assert_eq!(resolve_mode("connect https://example"), Mode::Builtin);
    assert_eq!(resolve_mode("disconnect"), Mode::Builtin);
    assert_eq!(resolve_mode("audit"), Mode::Builtin);
    assert_eq!(resolve_mode("rfc list"), Mode::Builtin);
    assert_eq!(resolve_mode("agent"), Mode::Builtin);
    assert_eq!(resolve_mode("stop 1"), Mode::Builtin);
}

#[test]
fn a_prefixed_names_are_not_builtins() {
    // names fall through to Contextual (regression pattern:
    // `init_is_not_a_builtin`).
    assert_eq!(resolve_mode("aroute"), Mode::Contextual);
    assert_eq!(resolve_mode("aattach 1"), Mode::Contextual);
    assert_eq!(resolve_mode("aconnect https://example"), Mode::Contextual);
    assert_eq!(resolve_mode("adisconnect"), Mode::Contextual);
}

#[test]
fn bare_system_owned_names_are_not_builtins() {
    // they leave the classifier (and then brush resolves them on PATH).
    assert_eq!(resolve_mode("route"), Mode::Contextual);
    assert_eq!(resolve_mode("route -n get default"), Mode::Contextual);
    assert_eq!(resolve_mode("login"), Mode::Contextual);
    assert_eq!(resolve_mode("log show --last 1m"), Mode::Contextual);
    // `logout` is untouched: no binary collision.
    assert_eq!(resolve_mode("logout"), Mode::Builtin);
}

#[test]
fn resolves_shell_via_bang() {
    assert_eq!(resolve_mode("!rm -rf /tmp/junk"), Mode::Shell);
    assert_eq!(resolve_mode("!ps"), Mode::Shell);
}

#[test]
fn resolves_contextual_for_bare_input() {
    assert_eq!(resolve_mode("ls -la"), Mode::Contextual);
    assert_eq!(resolve_mode("fix the auth bug"), Mode::Contextual);
    assert_eq!(resolve_mode(""), Mode::Contextual);
}

#[test]
fn classifies_path_first_token() {
    let c = HeuristicClassifier;
    assert_eq!(c.classify("./script.sh"), IntentGuess::Command);
}

#[test]
fn classifies_known_command() {
    let c = HeuristicClassifier;
    assert_eq!(c.classify("ls -la"), IntentGuess::Command);
    assert_eq!(c.classify("git status"), IntentGuess::Command);
}

#[test]
fn classifies_unknown_first_token_as_command() {
    // Unknown binaries fall through to Command — the shell surfaces a real
    // "command not found" error, matching zsh/bash UX. Agent routing requires
    // explicit intent (`@name`, or a trailing `?`).
    let c = HeuristicClassifier;
    assert_eq!(c.classify("fix the auth bug"), IntentGuess::Command);
}

#[test]
fn classifies_question_as_agent() {
    let c = HeuristicClassifier;
    assert_eq!(
        c.classify("what is the status of the deploy?"),
        IntentGuess::Agent
    );
}

#[test]
fn classifies_var_assignment_as_command() {
    let c = HeuristicClassifier;
    assert_eq!(c.classify("VAR=foo bar"), IntentGuess::Command);
}

#[test]
fn classifies_metachar_as_command() {
    let c = HeuristicClassifier;
    assert_eq!(c.classify("foo | bar"), IntentGuess::Command);
    assert_eq!(c.classify("echo $HOME"), IntentGuess::Command);
}

#[test]
fn resolves_tui_builtin() {
    // The `tui` keyword enters TUI mode at runtime — it must dispatch
    // as a builtin, not as a shell command (which would brush-127).
    assert_eq!(resolve_mode("tui"), Mode::Builtin);
    assert_eq!(resolve_mode("orkia tui"), Mode::Builtin);
    assert_eq!(resolve_mode("/tui"), Mode::Builtin);
}

#[test]
fn classifies_empty_as_agent() {
    let c = HeuristicClassifier;
    assert_eq!(c.classify(""), IntentGuess::Agent);
}

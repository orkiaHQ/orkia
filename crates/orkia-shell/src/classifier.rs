// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Classification: routes each line to shell, agent, or builtin.
//!
//! Stateless. brush handles unknown shell commands by returning exit 127;
//! the agent router can re-route on its own. The classifier only needs to
//! make a reasonable guess between `Command` and `Agent` for ambiguous
//! input — explicit prefixes (`!`, `@`, `orkia`, `/`) already take
//! precedence in `resolve_mode`.

pub use orkia_shell_types::classifier::*;

use std::sync::Arc;
use std::time::Duration;

use orkia_shell_types::{KernelRpc, KernelRpcError};
use parking_lot::RwLock;

use crate::decision::Mode;

pub struct HeuristicClassifier;

impl IntentClassifier for HeuristicClassifier {
    fn classify(&self, line: &str) -> IntentGuess {
        let trimmed = line.trim();

        // Empty input falls to the agent side (the prompt will no-op upstream).
        if trimmed.is_empty() {
            return IntentGuess::Agent;
        }

        // Question marks read as questions for the agent — but only when
        // the line has at least 2 characters of actual content. A bare
        // `?` (or `??`, etc.) is almost always curiosity / fat-finger
        // and shouldn't spawn a brand-new agent session with an empty
        // body. Require ≥ 2 non-`?` chars before the trailing `?`.
        // Exception: a trailing `$?` is the shell exit-status variable
        // (`echo $?`), not a question — leave it to brush.
        let body = trimmed.trim_end_matches('?');
        if trimmed.ends_with('?') && body.chars().count() >= 2 && !body.ends_with('$') {
            return IntentGuess::Agent;
        }

        // Everything else: brush handles it. If the first token is unknown
        // brush returns 127 — same UX as bash/zsh. (Lines reaching this
        // guess have already been screened by `try_parse_exec` and
        // `resolve_mode` in the REPL tick; this classifier never overrides
        // a grammar decision.)
        IntentGuess::Command
    }
}

fn is_builtin(name: &str) -> bool {
    // bare first tokens resolve to `Mode::Builtin`.
    crate::builtin_table::is_builtin(name)
}

pub fn resolve_mode(line: &str) -> Mode {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Mode::Contextual;
    }

    // @ prefix → agent delegation
    if let Some(rest) = trimmed.strip_prefix('@') {
        let name = rest.split_whitespace().next().unwrap_or("").to_string();
        return Mode::Agent(name);
    }

    // ! prefix → force shell passthrough
    if trimmed.starts_with('!') {
        return Mode::Shell;
    }

    // prefix is an explicit namespace claim, so an unknown remainder
    // resolves to Builtin and errors in-process via `dispatch_named`'s
    // catch-all — falling to Contextual would send the literal line to
    // brush, which execs a child orkia CLI.
    if let Some(rest) = trimmed.strip_prefix("orkia ") {
        // A flag-shaped head (`orkia --detach -c …`, `orkia --version`)
        // is the binary's own CLI syntax, not a builtin name — send it
        // to brush, which execs the real orkia binary.
        if rest.trim_start().starts_with('-') {
            return Mode::Shell;
        }
        // Same for a CLI-only verb (`orkia logs 1`, `orkia update`):
        // no REPL dispatch arm exists, the binary owns it. The namespace
        // claim still resolves in-house — brush execs the real binary.
        let head = rest.split_whitespace().next().unwrap_or("");
        if crate::builtin_table::is_cli_only(head) {
            return Mode::Shell;
        }
        return match resolve_mode(rest.trim()) {
            Mode::Contextual => Mode::Builtin,
            mode => mode,
        };
    }
    if trimmed == "orkia" {
        return Mode::Builtin;
    }

    // Optional `/` prefix → recurse on the rest
    if let Some(rest) = trimmed.strip_prefix('/') {
        return resolve_mode(rest.trim());
    }

    // First token → builtin lookup
    let first = trimmed.split_whitespace().next().unwrap_or("");
    if is_builtin(first) {
        return Mode::Builtin;
    }

    Mode::Contextual
}

/// ceiling at 30ms — short enough that a stuck kernel never feels
/// target p50 ≤ 60ms) usually wins the race once it's hot.
pub const ADAPTIVE_KERNEL_TIMEOUT: Duration = Duration::from_millis(30);

/// Layered classifier: tries the optional kernel first, falls back
/// to the heuristic on timeout, error, or absence. The kernel handle
/// is held behind an `RwLock<Option<Arc<dyn KernelRpc>>>` so the
/// capability resolver can swap it in/out at runtime — e.g. when
/// the user's plan changes — without rebuilding the REPL.
pub struct AdaptiveClassifier {
    heuristic: HeuristicClassifier,
    kernel: Arc<RwLock<Option<Arc<dyn KernelRpc>>>>,
    timeout: Duration,
}

impl AdaptiveClassifier {
    /// Build a classifier with no kernel attached. Behaves like
    /// [`HeuristicClassifier`] until [`AdaptiveHandle::set_kernel`]
    /// installs an RPC backend.
    pub fn heuristic_only() -> Self {
        Self {
            heuristic: HeuristicClassifier,
            kernel: Arc::new(RwLock::new(None)),
            timeout: ADAPTIVE_KERNEL_TIMEOUT,
        }
    }

    /// Build a classifier preloaded with a kernel handle. Equivalent
    /// to [`Self::heuristic_only`] followed by `set_kernel`.
    pub fn with_kernel(kernel: Arc<dyn KernelRpc>) -> Self {
        Self {
            heuristic: HeuristicClassifier,
            kernel: Arc::new(RwLock::new(Some(kernel))),
            timeout: ADAPTIVE_KERNEL_TIMEOUT,
        }
    }

    /// Return a cheap handle the capability resolver uses to swap
    /// the kernel in/out without taking ownership of the classifier.
    pub fn handle(&self) -> AdaptiveHandle {
        AdaptiveHandle {
            kernel: self.kernel.clone(),
        }
    }

    /// Override the per-call kernel timeout. Tests use this; runtime
    /// callers should leave the default.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl IntentClassifier for AdaptiveClassifier {
    fn classify(&self, line: &str) -> IntentGuess {
        let kernel = self.kernel.read().clone();
        if let Some(rpc) = kernel {
            match rpc.classify_with_timeout(line, self.timeout) {
                Ok(guess) => return guess,
                Err(KernelRpcError::Timeout) => {
                    tracing::debug!("adaptive: kernel timeout; falling back to heuristic");
                }
                Err(err) => {
                    tracing::debug!(error = %err, "adaptive: kernel error; falling back to heuristic");
                }
            }
        }
        self.heuristic.classify(line)
    }
}

/// Cheap, cloneable handle to the kernel slot inside an
/// [`AdaptiveClassifier`]. The capability resolver holds one and
/// calls `set_kernel`/`clear_kernel` from change subscribers.
#[derive(Clone)]
pub struct AdaptiveHandle {
    kernel: Arc<RwLock<Option<Arc<dyn KernelRpc>>>>,
}

impl AdaptiveHandle {
    pub fn set_kernel(&self, rpc: Arc<dyn KernelRpc>) {
        *self.kernel.write() = Some(rpc);
    }

    pub fn clear_kernel(&self) {
        *self.kernel.write() = None;
    }

    pub fn has_kernel(&self) -> bool {
        self.kernel.read().is_some()
    }

    /// Current kernel RPC handle, if connected. The native runtime
    /// uses this for `llm_complete`; `None` means the session must be
    /// refused (fail-closed — never a silent vendor fallback).
    pub fn kernel(&self) -> Option<Arc<dyn KernelRpc>> {
        self.kernel.read().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_shell_types::KernelVersion;

    struct AgentAlways;
    impl KernelRpc for AgentAlways {
        fn version(&self) -> KernelVersion {
            KernelVersion {
                protocol: 1,
                kernel: "test".into(),
                min_client: None,
                capabilities: Vec::new(),
            }
        }
        fn classify_with_timeout(
            &self,
            _line: &str,
            _t: Duration,
        ) -> Result<IntentGuess, KernelRpcError> {
            Ok(IntentGuess::Agent)
        }
        fn shutdown(&self) -> Result<(), KernelRpcError> {
            Ok(())
        }
    }

    struct AlwaysTimeout;
    impl KernelRpc for AlwaysTimeout {
        fn version(&self) -> KernelVersion {
            KernelVersion {
                protocol: 1,
                kernel: "test".into(),
                min_client: None,
                capabilities: Vec::new(),
            }
        }
        fn classify_with_timeout(
            &self,
            _line: &str,
            _t: Duration,
        ) -> Result<IntentGuess, KernelRpcError> {
            Err(KernelRpcError::Timeout)
        }
        fn shutdown(&self) -> Result<(), KernelRpcError> {
            Ok(())
        }
    }

    #[test]
    fn no_kernel_behaves_like_heuristic() {
        let c = AdaptiveClassifier::heuristic_only();
        // bare `?` stays a command per heuristic rules
        assert!(matches!(c.classify("?"), IntentGuess::Command));
        // real question routes to agent per heuristic rules
        assert!(matches!(c.classify("what is this?"), IntentGuess::Agent));
    }

    #[test]
    fn kernel_overrides_heuristic_on_success() {
        let c = AdaptiveClassifier::with_kernel(Arc::new(AgentAlways));
        // Heuristic would say Command; kernel returns Agent.
        assert!(matches!(c.classify("ls"), IntentGuess::Agent));
    }

    #[test]
    fn kernel_timeout_falls_back_to_heuristic() {
        let c = AdaptiveClassifier::with_kernel(Arc::new(AlwaysTimeout));
        // Falls back: heuristic on plain text → Command.
        assert!(matches!(c.classify("ls"), IntentGuess::Command));
    }

    #[test]
    fn handle_swap_changes_behaviour_live() {
        let c = AdaptiveClassifier::heuristic_only();
        assert!(matches!(c.classify("ls"), IntentGuess::Command));
        let h = c.handle();
        h.set_kernel(Arc::new(AgentAlways));
        assert!(matches!(c.classify("ls"), IntentGuess::Agent));
        h.clear_kernel();
        assert!(matches!(c.classify("ls"), IntentGuess::Command));
    }

    #[test]
    fn bare_question_mark_does_not_spawn_agent() {
        let c = HeuristicClassifier;
        // Single `?` is fat-finger / curiosity. Don't route to an
        // agent spawn — fall to brush which will return 127.
        assert!(matches!(c.classify("?"), IntentGuess::Command));
        assert!(matches!(c.classify("??"), IntentGuess::Command));
        assert!(matches!(c.classify("???"), IntentGuess::Command));
        assert!(matches!(c.classify("  ?  "), IntentGuess::Command));
    }

    #[test]
    fn real_question_routes_to_agent() {
        let c = HeuristicClassifier;
        assert!(matches!(c.classify("ok?"), IntentGuess::Agent));
        assert!(matches!(c.classify("what is this?"), IntentGuess::Agent));
        assert!(matches!(c.classify("hi?"), IntentGuess::Agent));
    }

    #[test]
    fn exit_status_variable_is_a_command() {
        let c = HeuristicClassifier;
        // `$?` is the shell exit-status variable, not a question.
        // Routing `echo $?` to an agent stalled the everyday-shell demo
        // on a trust modal (2026-06-10).
        assert!(matches!(c.classify("echo $?"), IntentGuess::Command));
        assert!(matches!(
            c.classify("test $? -eq 0 && echo ok"),
            IntentGuess::Command
        ));
        // But a real question that merely contains `$?` mid-line still routes.
        assert!(matches!(
            c.classify("what does $? mean in bash?"),
            IntentGuess::Agent
        ));
    }

    #[test]
    fn empty_routes_to_agent_noop() {
        let c = HeuristicClassifier;
        assert!(matches!(c.classify(""), IntentGuess::Agent));
        assert!(matches!(c.classify("   "), IntentGuess::Agent));
    }

    #[test]
    fn non_question_text_routes_to_command() {
        let c = HeuristicClassifier;
        assert!(matches!(c.classify("ls"), IntentGuess::Command));
        assert!(matches!(c.classify("cargo build"), IntentGuess::Command));
    }

    #[test]
    fn setup_resolves_to_builtin() {
        assert!(matches!(resolve_mode("setup"), Mode::Builtin));
        assert!(matches!(resolve_mode("orkia setup"), Mode::Builtin));
    }

    #[test]
    fn init_is_not_a_builtin() {
        // resolve to builtin mode — the name is vacated so it reaches
        // the contextual/shell path like any unknown command.
        assert!(matches!(resolve_mode("init"), Mode::Contextual));
    }

    #[test]
    fn top_is_not_a_builtin() {
        // shell path so /usr/bin/top runs instead of an Orkia error.
        assert!(matches!(resolve_mode("top"), Mode::Contextual));
    }

    #[test]
    fn stream_resolves_to_builtin() {
        // registration — it was unreachable. The table makes it real.
        assert!(matches!(resolve_mode("stream"), Mode::Builtin));
        assert!(matches!(resolve_mode("orkia stream"), Mode::Builtin));
    }

    #[test]
    fn orkia_prefixed_unknown_resolves_to_builtin() {
        // mode, never Contextual (which would spawn a child orkia CLI
        // through brush). Bare unknown names keep the contextual path.
        assert!(matches!(resolve_mode("orkia nosuchcmd"), Mode::Builtin));
        assert!(matches!(resolve_mode("nosuchcmd"), Mode::Contextual));
        // Explicit non-builtin sub-modes under the prefix are preserved.
        assert!(matches!(resolve_mode("orkia @faye hi"), Mode::Agent(_)));
        assert!(matches!(resolve_mode("orkia !ls"), Mode::Shell));
    }
}

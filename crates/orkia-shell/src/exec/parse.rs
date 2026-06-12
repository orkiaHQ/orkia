// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! `try_parse_exec` decides whether a line is a typed pipeline. It is the
//! gate that keeps POSIX pipes (`ls | grep`) on the legacy path: it returns
//! `Some` only when a contiguous run of registry commands appears. Stages to
//! the *left* of the first registry command form an external shell prefix
//! (the `Bytes → Value` boundary); stages to the *right* of the last
//! one form an external suffix (the `Value → Bytes` sink). A typed command
//! reappearing after the suffix begins makes the whole line fall through
//! (`None`).

use orkia_shell_types::{ExecError, ExecPlan, ParsedStage, Type};

use crate::exec::registry::CommandRegistry;
use crate::exec::tokenize::{split_pipeline, tokenize};

/// Strip an explicit `orkia `/`ork ` namespace prefix. Returns whether the
/// stage was namespaced and the remaining body.
fn strip_namespace(stage: &str) -> (bool, &str) {
    let s = stage.trim();
    for prefix in ["orkia ", "ork "] {
        if let Some(rest) = s.strip_prefix(prefix) {
            return (true, rest.trim_start());
        }
    }
    (false, s)
}

/// The typed command head of a stage (with its raw args), if it is one.
fn typed_head(stage: &str, registry: &CommandRegistry) -> Option<(String, Vec<String>)> {
    let (namespaced, body) = strip_namespace(stage);
    let tokens = tokenize(body);
    let head = tokens.first()?.clone();
    if !registry.contains(&head) {
        return None;
    }
    // The `orkia`/`ork` namespace always reaches the typed command.
    if !namespaced && crate::builtin_table::bare_typed_blocked(&head) {
        return None;
    }
    let args = tokens.get(1..).unwrap_or_default().to_vec();
    // the builtin grammar yields the line to brush — the system binary
    // owns that shape (`log show`, `route -n get default`).
    if !namespaced {
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        if matches!(
            crate::builtin_table::route_for(&head, &arg_refs),
            Some(crate::builtin_table::ShapeRoute::Brush)
        ) {
            return None;
        }
    }
    Some((head, args))
}

/// Parse `line` into an [`ExecPlan`] if it is a typed pipeline, else `None`.
pub fn try_parse_exec(line: &str, registry: &CommandRegistry) -> Option<ExecPlan> {
    let raw = line.trim();
    if raw.is_empty() {
        return None;
    }

    let stages = split_pipeline(raw);
    let heads: Vec<Option<(String, Vec<String>)>> =
        stages.iter().map(|s| typed_head(s, registry)).collect();

    let first_typed = heads.iter().position(Option::is_some)?;

    // The typed segment is the contiguous run of registry commands starting at
    // `first_typed`. Stages left of it form an external prefix (Bytes → Value),
    // stages right of it form an external suffix (Value → Bytes). A typed
    // command must not *reappear* after the external suffix begins — that
    // `external | typed | external | typed` shape is not supported; fall back.
    let mut last_typed = first_typed;
    while last_typed + 1 < heads.len() && heads[last_typed + 1].is_some() {
        last_typed += 1;
    }
    if heads[last_typed + 1..].iter().any(Option::is_some) {
        return None;
    }

    let shell_prefix = if first_typed == 0 {
        None
    } else {
        let prefix = &stages[..first_typed];
        // Agent stages on the left are handled by the agent-pipe path, not here.
        if prefix.iter().any(|s| s.trim_start().starts_with('@')) {
            return None;
        }
        Some(prefix.join(" | "))
    };

    let external_suffix = if last_typed + 1 < stages.len() {
        let suffix = &stages[last_typed + 1..];
        // An agent on the right is not a Value → Bytes hand-off; leave it.
        if suffix.iter().any(|s| s.trim_start().starts_with('@')) {
            return None;
        }
        Some(suffix.join(" | "))
    } else {
        None
    };

    let typed_stages: Vec<ParsedStage> = heads[first_typed..=last_typed]
        .iter()
        .filter_map(Option::as_ref)
        .map(|(name, raw_args)| ParsedStage {
            name: name.clone(),
            raw_args: raw_args.clone(),
        })
        .collect();

    Some(ExecPlan {
        shell_prefix,
        stages: typed_stages,
        external_suffix,
    })
}

/// Detect an agent whose output is piped into a downstream (non-agent)
/// command, and describe it as a [`ExecError::TypeMismatch`]. An interactive
/// agent emits a `ByteStream` (a rendered TUI), never the structured input a
/// command like `where` expects — so `@faye | where` and `cat | @faye | grep`
/// are refused. This is the type-driven generalization of the removed
/// `ParseError::AgentOnLeft` category rule. Agent-to-agent pipes (`@a | @b`)
/// are left alone (Team coordinator path).
pub fn agent_left_type_mismatch(line: &str, registry: &CommandRegistry) -> Option<ExecError> {
    let stages = split_pipeline(line);
    for pair in stages.windows(2) {
        let upstream = pair[0].trim_start();
        let downstream = pair[1].trim_start();
        if upstream.starts_with('@') && !downstream.starts_with('@') {
            let agent = upstream
                .trim_start_matches('@')
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            let (_, body) = strip_namespace(downstream);
            let command = tokenize(body).into_iter().next().unwrap_or_default();
            let expected = registry
                .signature(&command)
                .and_then(|sig| sig.io_types.first().map(|(input, _)| input.clone()))
                .unwrap_or(Type::Table);
            return Some(ExecError::TypeMismatch {
                command,
                expected,
                got: Type::ByteStream,
                upstream: format!("@{agent}"),
            });
        }
    }
    None
}

/// Classification of a line whose first `@agent` stage feeds a downstream
/// non-agent stage. Supersedes the boolean [`agent_left_type_mismatch`] for the
/// command is still a hard type error, but a downstream *external* shell command
/// (`tee`, `grep`, `wc`) is a valid sink — the agent's per-turn response text is
/// piped into it.
#[derive(Debug)]
pub enum AgentLeft {
    /// `@agent | <registry-cmd>` — refused. An interactive agent emits text,
    /// never the structured input a typed command expects.
    TypeMismatch(ExecError),
    /// `@agent [body] [--once] | <external-cmd...>` — bind the agent's per-turn
    /// response to the sink command (run per completed turn, fed the text on
    /// stdin). `sink_cmd` is the full remainder after the agent stage, so an
    /// internal pipeline (`grep x | wc -l`) is preserved and later run via
    /// `sh -c`.
    Sink {
        agent: String,
        body: String,
        once: bool,
        sink_cmd: String,
    },
    /// Not an agent-on-left pipe.
    NotAgentOnLeft,
}

/// Split an agent stage (`@faye review --once`) into `(agent, body, once)`.
/// `--once` is recognized as a standalone token anywhere in the body and
/// removed; the remaining words form the (whitespace-normalized) instruction.
/// Shared between the sink classifier and plain `@agent` dispatch
/// (`repl::prompt::parse_agent_or_pipeline`) so `--once` has one owner.
pub(crate) fn split_agent_stage(stage: &str) -> (String, String, bool) {
    let after_at = stage.trim_start().trim_start_matches('@');
    let mut words = after_at.split_whitespace();
    let agent = words.next().unwrap_or_default().to_string();
    let mut once = false;
    let mut body_words = Vec::new();
    for w in words {
        if w == "--once" {
            once = true;
        } else {
            body_words.push(w);
        }
    }
    (agent, body_words.join(" "), once)
}

/// Classify a line whose first `@agent` stage feeds a downstream non-agent
/// stage. See [`AgentLeft`]. Returns [`AgentLeft::NotAgentOnLeft`] when no
/// `@agent` directly precedes a non-`@` stage.
pub fn classify_agent_on_left(line: &str, registry: &CommandRegistry) -> AgentLeft {
    let stages = split_pipeline(line);
    for (i, pair) in stages.windows(2).enumerate() {
        let upstream = pair[0].trim_start();
        let downstream = pair[1].trim_start();
        if !upstream.starts_with('@') || downstream.starts_with('@') {
            continue;
        }
        let (agent, body, once) = split_agent_stage(upstream);
        let typed = typed_head(downstream, registry).is_some();
        // Sink only when the agent is the leftmost stage (no shell prefix to
        // feed it — that mixed shell→agent→shell shape stays a type error for
        // v1) and nothing downstream is itself an agent.
        let agent_is_first = i == 0;
        let downstream_has_agent = stages[i + 1..]
            .iter()
            .any(|s| s.trim_start().starts_with('@'));
        if !typed && agent_is_first && !downstream_has_agent {
            return AgentLeft::Sink {
                agent,
                body,
                once,
                sink_cmd: stages[i + 1..].join(" | "),
            };
        }
        // Preserve the existing type-error behavior for every other shape.
        let (_, dbody) = strip_namespace(downstream);
        let command = tokenize(dbody).into_iter().next().unwrap_or_default();
        let expected = registry
            .signature(&command)
            .and_then(|sig| sig.io_types.first().map(|(input, _)| input.clone()))
            .unwrap_or(Type::Table);
        return AgentLeft::TypeMismatch(ExecError::TypeMismatch {
            command,
            expected,
            got: Type::ByteStream,
            upstream: format!("@{agent}"),
        });
    }
    AgentLeft::NotAgentOnLeft
}

#[cfg(test)]
mod agent_sink_tests {
    use super::*;

    fn reg() -> CommandRegistry {
        CommandRegistry::with_pilots()
    }

    #[test]
    fn agent_into_external_is_sink() {
        match classify_agent_on_left("@faye review this | tee f", &reg()) {
            AgentLeft::Sink {
                agent,
                body,
                once,
                sink_cmd,
            } => {
                assert_eq!(agent, "faye");
                assert_eq!(body, "review this");
                assert!(!once);
                assert_eq!(sink_cmd, "tee f");
            }
            other => panic!("expected Sink, got {other:?}"),
        }
    }

    #[test]
    fn once_flag_parsed_and_stripped() {
        match classify_agent_on_left("@faye --once | wc -l", &reg()) {
            AgentLeft::Sink {
                agent,
                body,
                once,
                sink_cmd,
            } => {
                assert_eq!(agent, "faye");
                assert_eq!(body, "");
                assert!(once);
                assert_eq!(sink_cmd, "wc -l");
            }
            other => panic!("expected Sink, got {other:?}"),
        }
    }

    #[test]
    fn once_flag_after_body() {
        match classify_agent_on_left("@faye list the TODOs --once | tee todos.md", &reg()) {
            AgentLeft::Sink { body, once, .. } => {
                assert_eq!(body, "list the TODOs");
                assert!(once);
            }
            other => panic!("expected Sink, got {other:?}"),
        }
    }

    #[test]
    fn split_agent_stage_standalone_once() {
        // The plain `@agent` dispatch path (parse_agent_or_pipeline) reuses this
        // splitter, so a standalone `@faye review --once` (no `|`) parses the
        // body and the one-shot flag identically to the sink form.
        let (agent, body, once) = split_agent_stage("@faye review the auth module --once");
        assert_eq!(agent, "faye");
        assert_eq!(body, "review the auth module");
        assert!(once);

        let (agent, body, once) = split_agent_stage("@faye review the auth module");
        assert_eq!(agent, "faye");
        assert_eq!(body, "review the auth module");
        assert!(!once, "no --once → persistent");
    }

    #[test]
    fn agent_into_typed_command_is_type_mismatch() {
        match classify_agent_on_left("@faye | where status == working", &reg()) {
            AgentLeft::TypeMismatch(_) => {}
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn multistage_sink_preserved() {
        match classify_agent_on_left("@faye list | grep TODO | wc -l", &reg()) {
            AgentLeft::Sink { sink_cmd, .. } => assert_eq!(sink_cmd, "grep TODO | wc -l"),
            other => panic!("expected Sink, got {other:?}"),
        }
    }

    #[test]
    fn agent_to_agent_not_sink() {
        assert!(matches!(
            classify_agent_on_left("@a | @b", &reg()),
            AgentLeft::NotAgentOnLeft
        ));
    }

    #[test]
    fn quoted_pipe_not_a_split() {
        match classify_agent_on_left("@faye \"a | b\" | tee f", &reg()) {
            AgentLeft::Sink { body, sink_cmd, .. } => {
                assert_eq!(body, "\"a | b\"");
                assert_eq!(sink_cmd, "tee f");
            }
            other => panic!("expected Sink, got {other:?}"),
        }
    }

    #[test]
    fn shell_prefix_then_agent_then_external_stays_type_error() {
        // `printf x | @a | grep` — mixed shell→agent→shell. Not a clean sink
        // (agent is not leftmost). Preserve the type-error behavior for v1.
        match classify_agent_on_left("printf x | @a | grep bar", &reg()) {
            AgentLeft::TypeMismatch(_) => {}
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn plain_pipeline_not_agent_on_left() {
        assert!(matches!(
            classify_agent_on_left("ls | grep foo", &reg()),
            AgentLeft::NotAgentOnLeft
        ));
    }
}

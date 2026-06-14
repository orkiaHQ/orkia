// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file for terms.

//! Dependency-context composition (`SPEC-ORKIA-RFC-DISPATCH` §3.2).
//!
//! A task's effective prompt is its authored `body` plus the final responses
//! of everything it `depends_on`, injected as context. Each dependency's
//! response already lives, embedded, in that dependency's finished issue file
//! ([`crate::issues`]); the actor reads it from the store and hands it here.
//!
//! This module is a pure formatter — no I/O. Reading is the issue store's
//! job, and the store fails closed (§8) before composition is ever reached,
//! so by the time `compose_body` runs every dependency response is in hand.

/// One resolved dependency's context to inject: the response text the actor
/// read from that dependency's finished issue, labelled by task and agent.
#[derive(Debug, Clone)]
pub struct DepContext {
    pub task_id: String,
    pub agent: String,
    pub response: String,
}

/// Build the effective body: each dependency's response (in `deps` order)
/// under a header, then a separator, then the task's own body. With no
/// dependencies the body is returned unchanged.
pub fn compose_body(task_body: &str, deps: &[DepContext]) -> String {
    if deps.is_empty() {
        return task_body.to_string();
    }
    let mut out = String::new();
    for dep in deps {
        out.push_str(&format!(
            "## Context from @{} (dependency task `{}`)\n\n{}\n\n",
            dep.agent,
            dep.task_id,
            dep.response.trim_end()
        ));
    }
    out.push_str("---\n\n");
    out.push_str(task_body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(task_id: &str, agent: &str, response: &str) -> DepContext {
        DepContext {
            task_id: task_id.into(),
            agent: agent.into(),
            response: response.into(),
        }
    }

    #[test]
    fn no_deps_passes_body_through() {
        let body = "Design the API.";
        assert_eq!(compose_body(body, &[]), body);
    }

    #[test]
    fn single_dep_is_prepended_with_header() {
        let deps = vec![dep("t-api", "faye", "GET /things → [Thing]\n")];
        let composed = compose_body("Implement it.", &deps);
        assert!(composed.contains("## Context from @faye (dependency task `t-api`)"));
        assert!(composed.contains("GET /things → [Thing]"));
        assert!(composed.trim_end().ends_with("Implement it."));
    }

    #[test]
    fn multiple_deps_preserve_order() {
        let deps = vec![dep("t-a", "faye", "AAA"), dep("t-b", "sage", "BBB")];
        let composed = compose_body("join", &deps);
        let ia = composed.find("AAA").unwrap();
        let ib = composed.find("BBB").unwrap();
        assert!(ia < ib, "dependency order must be preserved");
    }
}

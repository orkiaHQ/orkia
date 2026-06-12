// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Vendor-agent provider identity and per-provider capability table.
//!
//! `ProviderId` is the single key every provider-conditional site matches
//! on (spawn env/args, hook install, trust pre-answer, cage corpus,
//! final-response extraction). The exhaustive `match` in
//! [`ProviderId::capabilities`] is the source of truth for what each
//! provider integration actually supports — a capability flips to `true`
//! only once a real-agent demos scenario proves it.
//! Unknown providers resolve to [`ProviderId::Generic`], which supports
//! nothing: fail-closed by construction.

/// Identity of the vendor CLI an agent job runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderId {
    Claude,
    Codex,
    Gemini,
    Kimi,
    /// Any command orkia has no integration knowledge for. Gets the
    /// filesystem context bundle and nothing else.
    Generic,
}

/// What orkia's integration with a provider has actually been proven to
/// do. Consulted as *policy* by the spawn planner ("do we deliver the
/// MCP config?"); the per-provider `match` arms stay the *mechanics*
/// ("how is it delivered?").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeCapabilities {
    /// Hook bridge captures tool-use / lifecycle events into the journal.
    pub hooks_capture: bool,
    /// The agent can reach orkia's MCP primitives (`recall`,
    /// `orkia_rfc_*`, knowledge tools) through a delivered MCP config.
    pub mcp_primitives: bool,
    /// PreToolUse mediation can deny a tool call cooperatively
    /// (Claude on macOS cage today).
    pub cooperative_deny: bool,
    /// A structured final response is captured at turn end
    /// (Stop hook → FinalResponseService).
    pub final_response: bool,
    /// No real-agent demos scenario has validated this provider yet —
    /// surfaces (pipelines, capability UI) must treat it as unproven.
    pub requires_real_agent_validation: bool,
}

impl ProviderId {
    /// Parse an explicit provider name (e.g. `[hooks] provider = "..."`).
    /// Case-insensitive; unknown names resolve to [`ProviderId::Generic`].
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" => Self::Claude,
            "codex" => Self::Codex,
            "gemini" => Self::Gemini,
            "kimi" => Self::Kimi,
            _ => Self::Generic,
        }
    }

    /// Derive the provider from a runtime command by basename, so
    /// `/usr/local/bin/claude` and `claude` resolve identically.
    pub fn from_command(command: &str) -> Self {
        let basename = command.trim().rsplit(['/', '\\']).next().unwrap_or(command);
        Self::parse(basename)
    }

    /// Resolve a job's provider identity: an explicit `[hooks] provider`
    /// wins over the command basename; with neither, `Generic`. Logs a
    /// warning when both are present and disagree — the explicit value
    /// still wins (the operator wrote it down on purpose).
    pub fn derive(explicit: Option<&str>, command: &str) -> Self {
        let from_command = Self::from_command(command);
        let Some(name) = explicit else {
            return from_command;
        };
        let from_explicit = Self::parse(name);
        if from_explicit != from_command && from_command != Self::Generic {
            tracing::warn!(
                explicit = name,
                command,
                "provider mismatch: [hooks] provider disagrees with runtime command; using the explicit value"
            );
        }
        from_explicit
    }

    /// Stable lowercase name. Matches the keys used by
    /// `orkia bridge --source <name>` and the final-response extractors.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Kimi => "kimi",
            Self::Generic => "generic",
        }
    }

    /// The capability table. Source of truth for every "does provider X
    /// support Y" decision. A `true` here is a claim backed by a green
    /// real-agent demos scenario — do not flip one speculatively.
    pub fn capabilities(self) -> RuntimeCapabilities {
        match self {
            Self::Claude => RuntimeCapabilities {
                hooks_capture: true,
                mcp_primitives: true,
                cooperative_deny: true,
                final_response: true,
                requires_real_agent_validation: false,
            },
            // Codex/Gemini hooks are recorder-only today. Codex MCP
            // delivery (per-invocation `-c mcp_servers.*` overrides) is
            // backed by the green real-codex demos scenario `codex-mcp`
            // (P1.8). Gemini's renderer exists but stays gated until a
            // real-gemini scenario passes (founder-blocked on auth —
            // see `_killix/P1.8-MCP-VENDOR-MECHANISMS.md`).
            Self::Codex => RuntimeCapabilities {
                hooks_capture: true,
                mcp_primitives: true,
                cooperative_deny: false,
                final_response: true,
                requires_real_agent_validation: false,
            },
            Self::Gemini => RuntimeCapabilities {
                hooks_capture: true,
                mcp_primitives: false,
                cooperative_deny: false,
                final_response: true,
                requires_real_agent_validation: false,
            },
            // Kimi has no known hook format, no MCP delivery, no
            // extractor: context bundle only, until proven otherwise.
            Self::Kimi => RuntimeCapabilities {
                hooks_capture: false,
                mcp_primitives: false,
                cooperative_deny: false,
                final_response: false,
                requires_real_agent_validation: true,
            },
            Self::Generic => RuntimeCapabilities {
                hooks_capture: false,
                mcp_primitives: false,
                cooperative_deny: false,
                final_response: false,
                requires_real_agent_validation: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_is_case_insensitive_and_fails_closed() {
        assert_eq!(ProviderId::parse("Claude"), ProviderId::Claude);
        assert_eq!(ProviderId::parse("CODEX"), ProviderId::Codex);
        assert_eq!(ProviderId::parse("gemini"), ProviderId::Gemini);
        assert_eq!(ProviderId::parse(" kimi "), ProviderId::Kimi);
        assert_eq!(ProviderId::parse("aider"), ProviderId::Generic);
        assert_eq!(ProviderId::parse(""), ProviderId::Generic);
    }

    #[test]
    fn from_command_uses_basename() {
        assert_eq!(
            ProviderId::from_command("/usr/local/bin/claude"),
            ProviderId::Claude
        );
        assert_eq!(ProviderId::from_command("kimi"), ProviderId::Kimi);
        assert_eq!(
            ProviderId::from_command("/opt/llm/bin/custom-agent"),
            ProviderId::Generic
        );
    }

    #[test]
    fn derive_prefers_explicit_over_basename() {
        // Explicit wins, even over a recognised command.
        assert_eq!(
            ProviderId::derive(Some("codex"), "/usr/bin/claude"),
            ProviderId::Codex
        );
        // Explicit fills in when the command says nothing.
        assert_eq!(
            ProviderId::derive(Some("claude"), "my-wrapper-script"),
            ProviderId::Claude
        );
        // No explicit → basename.
        assert_eq!(ProviderId::derive(None, "gemini"), ProviderId::Gemini);
        // Neither known → Generic.
        assert_eq!(ProviderId::derive(None, "aider"), ProviderId::Generic);
    }

    #[test]
    fn as_str_round_trips_through_parse() {
        for id in [
            ProviderId::Claude,
            ProviderId::Codex,
            ProviderId::Gemini,
            ProviderId::Kimi,
            ProviderId::Generic,
        ] {
            assert_eq!(ProviderId::parse(id.as_str()), id);
        }
    }

    /// Golden assertions of the capability table. A diff here is a
    /// product decision, not a refactor — it must be backed by a green
    /// real-agent demos scenario.
    #[test]
    fn capability_table_golden() {
        let claude = ProviderId::Claude.capabilities();
        assert!(claude.hooks_capture);
        assert!(claude.mcp_primitives);
        assert!(claude.cooperative_deny);
        assert!(claude.final_response);
        assert!(!claude.requires_real_agent_validation);

        let codex = ProviderId::Codex.capabilities();
        assert!(codex.hooks_capture);
        // P1.8 gate passed: demos scenario `codex-mcp` (real codex lists
        // the orkia MCP server and calls a tool through it).
        assert!(codex.mcp_primitives);
        assert!(!codex.cooperative_deny);
        assert!(codex.final_response);
        assert!(!codex.requires_real_agent_validation);

        let gemini = ProviderId::Gemini.capabilities();
        assert!(gemini.hooks_capture);
        assert!(!gemini.mcp_primitives, "Gemini: P1.8 gate not passed");
        assert!(!gemini.cooperative_deny);
        assert!(gemini.final_response);
        assert!(!gemini.requires_real_agent_validation);

        for id in [ProviderId::Kimi, ProviderId::Generic] {
            let caps = id.capabilities();
            assert!(!caps.hooks_capture);
            assert!(!caps.mcp_primitives);
            assert!(!caps.cooperative_deny);
            assert!(!caps.final_response);
            assert!(caps.requires_real_agent_validation);
        }
    }
}

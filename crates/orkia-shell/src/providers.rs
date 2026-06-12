// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Per-provider spawn plan: the single place that decides what
//! provider-specific env vars and CLI args a vendor agent spawns with.
//!
//! Both spawn paths (the REPL's `JobController::spawn` via
//! `inject_agent_context`, and the pipeline `StageExecutor::spawn_inputs`)
//! call [`build_spawn_plan`] instead of branching on `cmd == "claude"`.
//! The [`RuntimeCapabilities`] table decides *whether* something is
//! delivered (policy); the exhaustive provider `match` decides *how*
//! (mechanics). Pure function, no I/O — the Claude env/args byte order
//! is locked by the golden tests below.

use std::path::Path;

use orkia_shell_types::{ProviderId, RuntimeCapabilities};

/// What the caller has prepared for a vendor-agent spawn. Paths are
/// already written to disk; this function only decides which of them
/// the provider is told about, and how.
pub struct SpawnPlanInputs<'a> {
    pub provider: ProviderId,
    /// Fully assembled system prompt. Callers append any addendum
    /// (e.g. the pipeline protocol) *before* building the plan.
    pub assembled_system_prompt: Option<&'a str>,
    /// Path to the written `context.md` — exposed to every provider
    /// as `ORKIA_AGENT_CONTEXT`.
    pub context_path: Option<&'a Path>,
    /// Path to the written `mcp-config.json`. Only delivered to
    /// providers whose MCP integration is proven
    /// (`RuntimeCapabilities::mcp_primitives`).
    pub mcp_config_path: Option<&'a Path>,
    /// The parsed `mcpServers` object behind `mcp_config_path`.
    /// Providers without a config-file flag render delivery from this
    /// structured map instead (Codex `-c` overrides, Gemini settings
    /// merge) — see `_killix/P1.8-MCP-VENDOR-MECHANISMS.md`.
    pub mcp_servers: Option<&'a serde_json::Map<String, serde_json::Value>>,
    /// Caller wants PreToolUse mediation (a caged spawn on macOS).
    /// Honored only when the provider can cooperatively deny.
    pub mediate_requested: bool,
}

/// The provider-specific part of a spawn: env vars and args to append
/// after the caller's own argv, plus the resolved mediation decision.
pub struct ProviderSpawnPlan {
    pub env: Vec<(String, String)>,
    /// Appended after the agent's configured args — order within this
    /// vec is part of the provider contract (golden-tested).
    pub args: Vec<String>,
    /// `Some` when the provider's MCP delivery is a project-settings
    /// merge the caller must perform (Gemini: merge these entries into
    /// the `mcpServers` key of `.gemini/settings.json` under the
    /// agent's working directory, via `hooks::merge_mcp_servers`).
    /// Pure data — this builder does no I/O.
    pub gemini_mcp_servers: Option<serde_json::Map<String, serde_json::Value>>,
    /// `mediate_requested && capabilities.cooperative_deny`.
    pub mediate: bool,
    pub capabilities: RuntimeCapabilities,
}

pub fn build_spawn_plan(inputs: SpawnPlanInputs<'_>) -> ProviderSpawnPlan {
    let capabilities = inputs.provider.capabilities();
    let mut env: Vec<(String, String)> = Vec::new();
    let mut args: Vec<String> = Vec::new();
    let mut gemini_mcp_servers = None;

    // Every provider gets the filesystem context bundle.
    if let Some(path) = inputs.context_path {
        env.push((
            "ORKIA_AGENT_CONTEXT".into(),
            path.to_string_lossy().into_owned(),
        ));
    }

    // Delivery mechanics per provider — exhaustive, so adding a
    // `ProviderId` variant forces a delivery decision here.
    match inputs.provider {
        ProviderId::Claude => {
            if let Some(prompt) = inputs.assembled_system_prompt {
                env.push(("CLAUDE_SYSTEM_PROMPT".into(), prompt.to_owned()));
            }
            if capabilities.mcp_primitives
                && let Some(path) = inputs.mcp_config_path
            {
                args.push("--mcp-config".into());
                args.push(path.to_string_lossy().into_owned());
            }
        }
        // Per-invocation dotted-path TOML overrides — ephemeral, no
        // `~/.codex/config.toml` mutation (validated against codex-cli
        // 0.137.0; see `_killix/P1.8-MCP-VENDOR-MECHANISMS.md`).
        ProviderId::Codex => {
            if capabilities.mcp_primitives
                && let Some(servers) = inputs.mcp_servers
            {
                args.extend(codex_mcp_override_args(servers));
            }
        }
        // Gemini has no per-invocation config flag: delivery is a
        // project-scope settings merge the I/O-owning caller performs.
        ProviderId::Gemini => {
            if capabilities.mcp_primitives
                && let Some(servers) = inputs.mcp_servers
            {
                gemini_mcp_servers = Some(servers.clone());
            }
        }
        // No proven delivery mechanism: context bundle only.
        ProviderId::Kimi | ProviderId::Generic => {}
    }

    ProviderSpawnPlan {
        env,
        args,
        gemini_mcp_servers,
        mediate: inputs.mediate_requested && capabilities.cooperative_deny,
        capabilities,
    }
}

/// Render the `-c mcp_servers.<name>.<field>=<toml>` override pairs a
/// codex spawn carries. JSON string/array escaping is a valid subset of
/// TOML basic-string/array syntax, so values are rendered through
/// `serde_json`. Stdio entries only — codex's url-based MCP support is
/// unverified, and every orkia-delivered server (knowledge bridge,
/// pipe) is stdio.
fn codex_mcp_override_args(servers: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let mut args = Vec::new();
    for (name, entry) in servers {
        let Some(command) = entry.get("command").and_then(|v| v.as_str()) else {
            continue;
        };
        let key = toml_key(name);
        args.push("-c".into());
        args.push(format!(
            "mcp_servers.{key}.command={}",
            toml_value_from_str(command)
        ));
        if let Some(list) = entry.get("args").and_then(|v| v.as_array())
            && !list.is_empty()
        {
            args.push("-c".into());
            args.push(format!(
                "mcp_servers.{key}.args={}",
                serde_json::Value::Array(list.clone())
            ));
        }
        if let Some(env) = entry.get("env").and_then(|v| v.as_object())
            && !env.is_empty()
        {
            let pairs: Vec<String> = env
                .iter()
                .map(|(k, v)| {
                    let value = match v {
                        serde_json::Value::String(s) => toml_value_from_str(s),
                        other => other.to_string(),
                    };
                    format!("{} = {value}", toml_key(k))
                })
                .collect();
            args.push("-c".into());
            args.push(format!("mcp_servers.{key}.env={{{}}}", pairs.join(", ")));
        }
    }
    args
}

/// A TOML key segment: bare when it only uses bare-key characters,
/// quoted (JSON string escaping == TOML basic-string escaping)
/// otherwise. Server names and env keys come from agent.toml — treat
/// them as untrusted bytes.
fn toml_key(name: &str) -> String {
    let bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        name.to_string()
    } else {
        toml_value_from_str(name)
    }
}

/// A TOML basic string via JSON serialization (`Display` for
/// `serde_json::Value` is infallible).
fn toml_value_from_str(s: &str) -> String {
    serde_json::Value::String(s.to_owned()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn full_inputs(provider: ProviderId) -> (PathBuf, PathBuf) {
        let _ = provider;
        (
            PathBuf::from("/run/context.md"),
            PathBuf::from("/run/mcp-config.json"),
        )
    }

    /// Golden: the exact env/args Claude spawns with, in order. This is
    /// the byte-identical contract with the pre-refactor `if cmd ==
    /// "claude"` branches — a diff here is a Claude regression.
    #[test]
    fn claude_plan_env_and_args_are_byte_identical() {
        let (ctx, mcp) = full_inputs(ProviderId::Claude);
        let plan = build_spawn_plan(SpawnPlanInputs {
            provider: ProviderId::Claude,
            assembled_system_prompt: Some("you are faye"),
            context_path: Some(&ctx),
            mcp_config_path: Some(&mcp),
            mcp_servers: None,
            mediate_requested: false,
        });
        assert_eq!(
            plan.env,
            vec![
                (
                    "ORKIA_AGENT_CONTEXT".to_string(),
                    "/run/context.md".to_string()
                ),
                (
                    "CLAUDE_SYSTEM_PROMPT".to_string(),
                    "you are faye".to_string()
                ),
            ]
        );
        assert_eq!(plan.args, vec!["--mcp-config", "/run/mcp-config.json"]);
    }

    #[test]
    fn claude_omits_what_was_not_prepared() {
        let plan = build_spawn_plan(SpawnPlanInputs {
            provider: ProviderId::Claude,
            assembled_system_prompt: None,
            context_path: None,
            mcp_config_path: None,
            mcp_servers: None,
            mediate_requested: false,
        });
        assert!(plan.env.is_empty());
        assert!(plan.args.is_empty());
    }

    #[test]
    fn other_providers_get_the_context_bundle_only() {
        for provider in [
            ProviderId::Codex,
            ProviderId::Gemini,
            ProviderId::Kimi,
            ProviderId::Generic,
        ] {
            let (ctx, mcp) = full_inputs(provider);
            let plan = build_spawn_plan(SpawnPlanInputs {
                provider,
                assembled_system_prompt: Some("ignored"),
                context_path: Some(&ctx),
                mcp_config_path: Some(&mcp),
                mcp_servers: None,
                mediate_requested: false,
            });
            assert_eq!(
                plan.env,
                vec![(
                    "ORKIA_AGENT_CONTEXT".to_string(),
                    "/run/context.md".to_string()
                )],
                "{provider:?}"
            );
            assert!(plan.args.is_empty(), "{provider:?}");
        }
    }

    #[test]
    fn mediate_requires_cooperative_deny() {
        let request = |provider, requested| {
            build_spawn_plan(SpawnPlanInputs {
                provider,
                assembled_system_prompt: None,
                context_path: None,
                mcp_config_path: None,
                mcp_servers: None,
                mediate_requested: requested,
            })
            .mediate
        };
        assert!(request(ProviderId::Claude, true));
        assert!(!request(ProviderId::Claude, false));
        // Codex cannot cooperatively deny — a mediation request is
        // dropped, not silently pretended.
        assert!(!request(ProviderId::Codex, true));
    }

    fn sample_servers() -> serde_json::Map<String, serde_json::Value> {
        let value = serde_json::json!({
            "orkia-knowledge": {
                "command": "/usr/local/bin/orkia",
                "args": ["knowledge-mcp"],
                "env": {"ORKIA_SOCKET_PATH": "/tmp/orkia.sock", "ORKIA_JOB_ID": "42"},
            },
            "remote-only": { "url": "https://mcp.example.com/sse" },
        });
        match value {
            serde_json::Value::Object(map) => map,
            _ => unreachable!(),
        }
    }

    /// Golden: the exact `-c` override pairs codex spawns with —
    /// validated against codex-cli 0.137.0 (`codex -c ... mcp list`
    /// shows the injected server; P1.8 mechanisms note).
    #[test]
    fn codex_override_args_golden() {
        let args = codex_mcp_override_args(&sample_servers());
        assert_eq!(
            args,
            vec![
                "-c".to_string(),
                "mcp_servers.orkia-knowledge.command=\"/usr/local/bin/orkia\"".to_string(),
                "-c".to_string(),
                "mcp_servers.orkia-knowledge.args=[\"knowledge-mcp\"]".to_string(),
                "-c".to_string(),
                "mcp_servers.orkia-knowledge.env={ORKIA_JOB_ID = \"42\", ORKIA_SOCKET_PATH = \"/tmp/orkia.sock\"}"
                    .to_string(),
            ],
            "url-only entries are skipped; stdio entry renders command/args/env"
        );
    }

    #[test]
    fn codex_override_args_quote_non_bare_names() {
        let value = serde_json::json!({
            "weird name.v1": { "command": "tool" },
        });
        let serde_json::Value::Object(servers) = value else {
            unreachable!()
        };
        let args = codex_mcp_override_args(&servers);
        assert_eq!(args[1], "mcp_servers.\"weird name.v1\".command=\"tool\"");
    }

    /// Codex's gate is flipped (P1.8: green `codex-mcp` demos scenario):
    /// a populated `mcp_servers` input renders the `-c` override args on
    /// the spawn plan itself.
    #[test]
    fn codex_plan_renders_mcp_overrides() {
        let servers = sample_servers();
        let plan = build_spawn_plan(SpawnPlanInputs {
            provider: ProviderId::Codex,
            assembled_system_prompt: None,
            context_path: None,
            mcp_config_path: None,
            mcp_servers: Some(&servers),
            mediate_requested: false,
        });
        assert_eq!(plan.args, codex_mcp_override_args(&servers));
        assert!(!plan.args.is_empty());
        assert!(plan.gemini_mcp_servers.is_none());
    }

    /// The capability gate, not the renderer, decides delivery: while
    /// `mcp_primitives` is false for codex/gemini (the P1.8 real-agent
    /// demos gate), a populated `mcp_servers` input must change nothing.
    #[test]
    fn mcp_delivery_is_capability_gated() {
        let servers = sample_servers();
        for provider in [ProviderId::Codex, ProviderId::Gemini] {
            if provider.capabilities().mcp_primitives {
                continue; // gate flipped: covered by the golden tests
            }
            let plan = build_spawn_plan(SpawnPlanInputs {
                provider,
                assembled_system_prompt: None,
                context_path: None,
                mcp_config_path: None,
                mcp_servers: Some(&servers),
                mediate_requested: false,
            });
            assert!(plan.args.is_empty(), "{provider:?}");
            assert!(plan.gemini_mcp_servers.is_none(), "{provider:?}");
        }
    }
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use crate::agent::AgentInfo;
use crate::agent_dir;
use orkia_shell_types::{AgentStatus, Scope};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct AgentCommandConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// launches agents through `<bin> --policy <policy> -- <agent> …` instead of
/// the bare vendor binary. Default off: a config with no `[cage]` block (or
/// `enabled = false`) behaves exactly as before. The cage enforces per-OS
/// platforms it is passthrough.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct CageConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Path to the TOML policy file passed to `<bin> --policy`. A leading
    /// `~/` is expanded to `$HOME`. When `enabled` but unset, the cage is
    /// not applied (no policy → nothing to enforce).
    #[serde(default)]
    pub policy: Option<PathBuf>,
    /// Cage launcher binary, resolved via `$PATH` when bare. Defaults to
    /// `orkia-cage`.
    #[serde(default)]
    pub bin: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct DaemonConfig {
    #[serde(default = "default_daemon_ipc_timeout_ms")]
    pub ipc_timeout_ms: u64,
    #[serde(default = "default_daemon_startup_timeout_ms")]
    pub startup_timeout_ms: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            ipc_timeout_ms: default_daemon_ipc_timeout_ms(),
            startup_timeout_ms: default_daemon_startup_timeout_ms(),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ShellConfig {
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// Legacy inline agent list. Populated only when no `~/.orkia/agents/`
    /// directory exists yet — kept for backwards-compat during the V2 →
    /// V4 migration window. Once the directory is scaffolded, agents are
    /// loaded from the filesystem.
    #[serde(default)]
    pub agents: Vec<AgentInfo>,
    /// Legacy inline `[agent_commands.*]` mapping. Same lifecycle as `agents`.
    #[serde(default)]
    pub agent_commands: HashMap<String, AgentCommandConfig>,
    /// Names of agents whose `[runtime] type = "native"`. Derived from
    /// the agents directory at hydrate time, never read from config.toml.
    /// These have no vendor command (excluded from `agent_commands`);
    /// dispatch routes them to the native runtime (Part 2) instead.
    #[serde(skip)]
    pub native_agents: std::collections::HashSet<String>,
    #[serde(default)]
    pub default_shell: Option<String>,
    #[serde(default)]
    pub default_project: Option<String>,
    /// Default visibility scope inherited by any project, RFC, or issue
    /// that does not declare its own. Absent means the effective default
    /// PR1b ships the field as foundation only — nothing reads it yet.
    #[serde(default)]
    pub default_scope: Option<Scope>,
    /// `"shell"` (default) for stdin/stdout shell mode, `"tui"` for the
    /// ratatui alternate-screen experience at launch. Overridden by
    /// `--tui` / `--no-tui` on the command line.
    #[serde(default)]
    pub default_mode: Option<String>,
    /// Source `~/.bashrc` on startup (non-login shells). Defaults to
    /// true. Set to `false` if your `.bashrc` does bash-version checks
    /// that fail on brush.
    #[serde(default)]
    pub load_bashrc: Option<bool>,
    /// Source the profile chain (`.bash_profile`, `.bash_login`,
    /// `.profile`) for login shells. Defaults to true.
    #[serde(default)]
    pub load_profile: Option<bool>,
    /// Verbosity of journal-driven notifications between prompts.
    /// One of `"full"` (every tool use, completion, approval),
    /// `"summary"` (completions + approvals only), or `"silent"`
    /// (none — use the `journal` builtin). Defaults to `"full"`.
    #[serde(default)]
    pub notification_verbosity: Option<String>,
    /// config without a `[cage]` block spawns agents exactly as before.
    #[serde(default)]
    pub cage: CageConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
}

impl ShellConfig {
    pub fn load() -> Self {
        let config_path = dirs_home().join(".orkia").join("config.toml");
        let mut cfg = if config_path.exists() {
            let content = std::fs::read_to_string(&config_path).unwrap_or_default();
            match toml::from_str::<ShellConfig>(&content) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to parse config.toml, using defaults");
                    Self::default()
                }
            }
        } else {
            Self::default()
        };
        cfg.hydrate_agents_from_dir();
        cfg
    }

    /// Populate `agents` from `<data_dir>/agents/`. If the directory
    /// exists this wins over any inline `[agents.*]` entries. The inline
    /// list is left alone otherwise so legacy users keep working until
    /// migration runs.
    pub fn hydrate_agents_from_dir(&mut self) {
        let defs = agent_dir::load_all_definitions(&self.data_dir);
        if defs.is_empty() {
            return;
        }
        self.agents = defs
            .iter()
            .map(|d| agent_dir::to_agent_info(d, AgentStatus::Idle))
            .collect();
        // Rebuild the legacy command map from the directory entries so
        // `resolve_agent` keeps working without touching the rest of the
        // codebase. Native-runtime agents have no vendor command: they go
        // into `native_agents` instead so dispatch can route (or refuse
        // with a clear reason) by name.
        self.native_agents = defs
            .iter()
            .filter(|d| {
                matches!(
                    d.runtime,
                    orkia_shell_types::AgentRuntimeKind::Native { .. }
                )
            })
            .map(|d| d.name.clone())
            .collect();
        self.agent_commands = defs
            .into_iter()
            .filter(|d| {
                matches!(
                    d.runtime,
                    orkia_shell_types::AgentRuntimeKind::Vendor { .. }
                )
            })
            .map(|d| {
                (
                    d.name,
                    AgentCommandConfig {
                        command: d.command,
                        args: d.args,
                    },
                )
            })
            .collect();
    }

    /// Human-readable reason why [`Self::resolve_agent`] returned `None`
    /// for `name`. Distinguishes "native runtime on a vendor-only path"
    /// from the plain unknown-agent case so dispatch sites surface the
    /// right hint. Direct `@name` dispatch forks to the native loop
    /// before command resolution, so only the paths that haven't grown a
    /// native arm yet (shell→agent pipe, RFC delegation) reach this.
    pub fn agent_unresolved_reason(&self, name: &str) -> String {
        if self.native_agents.contains(name) {
            format!(
                "agent '{name}' uses [runtime] type = \"native\" — this dispatch path \
                 doesn't support native sessions yet (use `@{name} <prompt>` directly)"
            )
        } else {
            format!("no command configured for '{name}'")
        }
    }
}

impl ShellConfig {
    pub fn resolve_agent(&self, name: &str) -> Option<(&str, &[String])> {
        self.agent_commands
            .get(name)
            .map(|c| (c.command.as_str(), c.args.as_slice()))
    }
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            agents: Vec::new(),
            agent_commands: HashMap::new(),
            native_agents: std::collections::HashSet::new(),
            default_shell: None,
            default_project: None,
            default_scope: None,
            default_mode: None,
            load_bashrc: None,
            load_profile: None,
            notification_verbosity: None,
            cage: CageConfig::default(),
            daemon: DaemonConfig::default(),
        }
    }
}

/// Notification verbosity, parsed from `ShellConfig.notification_verbosity`.
/// `Full` is the default — every user-facing journal envelope produces a
/// pre-prompt notification line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NotificationVerbosity {
    #[default]
    Full,
    Summary,
    Silent,
}

impl NotificationVerbosity {
    pub fn parse(s: Option<&str>) -> Self {
        match s.map(str::to_ascii_lowercase).as_deref() {
            Some("silent") => NotificationVerbosity::Silent,
            Some("summary") => NotificationVerbosity::Summary,
            // "full" or unset or anything weird → full
            _ => NotificationVerbosity::Full,
        }
    }
}

fn default_data_dir() -> PathBuf {
    dirs_home().join(".orkia")
}

fn default_daemon_ipc_timeout_ms() -> u64 {
    250
}

fn default_daemon_startup_timeout_ms() -> u64 {
    1000
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbosity_defaults_to_full_when_unset() {
        assert_eq!(
            NotificationVerbosity::parse(None),
            NotificationVerbosity::Full
        );
    }

    #[test]
    fn verbosity_parses_known_values() {
        assert_eq!(
            NotificationVerbosity::parse(Some("silent")),
            NotificationVerbosity::Silent
        );
        assert_eq!(
            NotificationVerbosity::parse(Some("SUMMARY")),
            NotificationVerbosity::Summary
        );
        assert_eq!(
            NotificationVerbosity::parse(Some("full")),
            NotificationVerbosity::Full
        );
    }

    #[test]
    fn verbosity_unknown_falls_back_to_full() {
        assert_eq!(
            NotificationVerbosity::parse(Some("nope")),
            NotificationVerbosity::Full
        );
    }

    #[test]
    fn default_scope_parses_from_config_toml() {
        let body = "default_scope = \"public\"\n";
        let cfg: ShellConfig = toml::from_str(body).expect("parse");
        assert_eq!(cfg.default_scope, Some(Scope::Public));
    }

    #[test]
    fn default_scope_is_none_when_unset() {
        let cfg: ShellConfig = toml::from_str("").expect("parse empty");
        assert_eq!(cfg.default_scope, None);
    }

    #[test]
    fn cage_defaults_off_when_absent() {
        let cfg: ShellConfig = toml::from_str("").expect("parse empty");
        assert!(!cfg.cage.enabled);
        assert!(cfg.cage.policy.is_none());
        assert!(cfg.cage.bin.is_none());
    }

    #[test]
    fn cage_block_parses() {
        let body = "[cage]\nenabled = true\npolicy = \"/x/policy.toml\"\n";
        let cfg: ShellConfig = toml::from_str(body).expect("parse cage block");
        assert!(cfg.cage.enabled);
        assert_eq!(
            cfg.cage.policy.as_deref(),
            Some(std::path::Path::new("/x/policy.toml"))
        );
        assert!(cfg.cage.bin.is_none());
    }

    #[test]
    fn legacy_backend_url_field_is_silently_ignored() {
        // Users migrating from the pre-PR1b config schema may still have
        // `backend_url = "..."` in their config.toml. The deserialiser
        // does not opt into `deny_unknown_fields`, so the line must be
        // silently ignored and the rest of the file must still parse.
        let body = "backend_url = \"https://stale.example\"\ndefault_project = \"x\"\n";
        let cfg: ShellConfig = toml::from_str(body).expect("legacy config still parses");
        assert_eq!(cfg.default_project.as_deref(), Some("x"));
    }
}

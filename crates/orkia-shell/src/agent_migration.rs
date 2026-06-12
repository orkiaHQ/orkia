// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! One-shot migration of legacy `[agents.*]`/`[agent_commands.*]`
//! entries in `~/.orkia/config.toml` into directory-based agents under

use crate::config::{AgentCommandConfig, ShellConfig};

/// Migrate inline agent entries to directory form. Returns the list of
/// agent names that were scaffolded so the caller can surface a
/// one-time notice. Idempotent: a no-op when the agents dir already
/// exists or there is nothing to migrate.
pub fn migrate_legacy_agents(config: &ShellConfig) -> Vec<String> {
    let agents_dir = crate::agent_dir::agents_root(&config.data_dir);
    if agents_dir.exists() {
        return Vec::new();
    }
    if config.agents.is_empty() && config.agent_commands.is_empty() {
        return Vec::new();
    }
    if std::fs::create_dir_all(&agents_dir).is_err() {
        return Vec::new();
    }

    let mut names: Vec<String> = config
        .agents
        .iter()
        .map(|a| a.name.clone())
        .chain(config.agent_commands.keys().cloned())
        .collect();
    names.sort();
    names.dedup();

    let mut migrated = Vec::new();
    for name in names {
        let cmd = config
            .agent_commands
            .get(&name)
            .cloned()
            .unwrap_or_else(|| AgentCommandConfig {
                command: "claude".into(),
                args: Vec::new(),
            });
        let legacy = config.agents.iter().find(|a| a.name == name);
        let archetype = legacy
            .map(|a| a.archetype.clone())
            .unwrap_or_else(|| "general".into());

        let dir = agents_dir.join(&name);
        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let agent_toml = render_agent_toml(&name, &archetype, &cmd);
        if std::fs::write(dir.join("agent.toml"), agent_toml).is_err() {
            continue;
        }
        let _ = std::fs::write(
            dir.join("system-prompt.md"),
            format!("# {name}\n\n(Edit this file to define the agent's behaviour.)\n"),
        );
        let _ = std::fs::write(
            dir.join("memory.md"),
            format!(
                "# Memory\n\n## {today}\n- Migrated from config.toml\n",
                today = chrono::Utc::now().format("%Y-%m-%d"),
            ),
        );
        let _ = std::fs::write(
            dir.join("tools.toml"),
            "# MCP servers and tools for this agent\n",
        );
        migrated.push(name);
    }
    migrated
}

/// Escape a string for a TOML basic (double-quoted) string. Backslash MUST be
/// escaped before the quote, else a value ending in `\` would swallow the
/// closing quote and a Windows path would corrupt the file (BUG-037).
fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn render_agent_toml(name: &str, archetype: &str, cmd: &AgentCommandConfig) -> String {
    let args = cmd
        .args
        .iter()
        .map(|a| format!("\"{}\"", toml_escape(a)))
        .collect::<Vec<_>>()
        .join(", ");
    // per-agent scalar is gone. `AgentTrustSection` still parses (config compat),
    // defaulting when absent.
    format!(
        "[agent]\nname = \"{name}\"\narchetype = \"{archetype}\"\n\n[runtime]\ncommand = \"{cmd}\"\nargs = [{args}]\n\n[projects]\nassigned = []\n\n[context]\nmax_context_tokens = 4000\ninclude_rfcs = true\ninclude_issues = true\n",
        name = toml_escape(name),
        archetype = toml_escape(archetype),
        cmd = toml_escape(&cmd.command),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentCommandConfig;
    use orkia_shell_types::{AgentInfo, AgentStatus};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use uuid::Uuid;

    fn legacy_config(data_dir: PathBuf) -> ShellConfig {
        let mut agent_commands = HashMap::new();
        agent_commands.insert(
            "faye".to_string(),
            AgentCommandConfig {
                command: "claude".into(),
                args: vec!["--model".into(), "sonnet".into()],
            },
        );
        let agent = AgentInfo {
            id: Uuid::nil(),
            name: "faye".into(),
            archetype: "software-eng".into(),
            status: AgentStatus::Idle,
            model: "claude".into(),
            dir: PathBuf::new(),
            description: None,
            command: "claude".into(),
            args: Vec::new(),
            assigned_projects: Vec::new(),
            max_context_tokens: 4000,
        };
        ShellConfig {
            data_dir,
            agents: vec![agent],
            agent_commands,
            ..ShellConfig::default()
        }
    }

    #[test]
    fn migrates_legacy_agents() {
        let tmp = TempDir::new().unwrap();
        let config = legacy_config(tmp.path().to_path_buf());
        let migrated = migrate_legacy_agents(&config);
        assert_eq!(migrated, vec!["faye".to_string()]);
        let faye_dir = tmp.path().join("agents").join("faye");
        assert!(faye_dir.join("agent.toml").exists());
        assert!(faye_dir.join("system-prompt.md").exists());
        assert!(faye_dir.join("memory.md").exists());
        assert!(faye_dir.join("tools.toml").exists());
        let content = std::fs::read_to_string(faye_dir.join("agent.toml")).unwrap();
        assert!(content.contains("name = \"faye\""));
        assert!(content.contains("archetype = \"software-eng\""));
        // per-agent scalar is removed; AgentTrustSection defaults when absent.
        assert!(!content.contains("[trust]"));
    }

    #[test]
    fn migration_is_idempotent_when_dir_exists() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("agents")).unwrap();
        let config = legacy_config(tmp.path().to_path_buf());
        assert!(migrate_legacy_agents(&config).is_empty());
    }

    #[test]
    fn no_op_when_nothing_to_migrate() {
        let tmp = TempDir::new().unwrap();
        let config = ShellConfig {
            data_dir: tmp.path().to_path_buf(),
            ..ShellConfig::default()
        };
        assert!(migrate_legacy_agents(&config).is_empty());
    }
}

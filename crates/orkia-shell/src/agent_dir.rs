// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Loader for filesystem agent definitions under `~/.orkia/agents/`.
//!
//! `agent.toml` (required) plus the optional `system-prompt.md`,
//! `memory.md`, `tools.toml`. Bad/missing `agent.toml` skips the agent
//! and logs a warning rather than failing shell startup.

use std::path::{Path, PathBuf};

use orkia_shell_types::{AgentConfigFile, AgentDefinition, AgentInfo, AgentStatus, AgentToolsFile};
use uuid::Uuid;

/// Path to the agent directory root.
pub fn agents_root(data_dir: &Path) -> PathBuf {
    data_dir.join("agents")
}

/// Path to an agent's per-agent cage policy: `<data_dir>/agents/<name>/policy.toml`.
///
/// This is the storage substrate for `cap @<name>` — each agent owns a distinct
/// policy file the `cap` builtin mutates in place. The cage prefers it over the
/// global `[cage].policy`; absence means "no per-agent policy, fall back".
pub fn agent_policy_path(data_dir: &Path, name: &str) -> PathBuf {
    agents_root(data_dir).join(name).join("policy.toml")
}

/// Scan `<data_dir>/agents/` and load every directory whose
/// `agent.toml` parses. Skips `_default/` for list purposes (kept for
/// fallback by callers via [`load_agent`] directly).
pub fn load_all_definitions(data_dir: &Path) -> Vec<AgentDefinition> {
    let root = agents_root(data_dir);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut defs: Vec<AgentDefinition> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| !n.starts_with('_'))
                .unwrap_or(false)
        })
        .filter_map(|e| load_definition(&e.path()))
        .collect();
    defs.sort_by(|a, b| a.name.cmp(&b.name));
    defs
}

/// Load a single agent directory. Returns `None` if `agent.toml` is
/// absent or fails to parse.
pub fn load_definition(dir: &Path) -> Option<AgentDefinition> {
    let toml_path = dir.join("agent.toml");
    let content = match std::fs::read_to_string(&toml_path) {
        Ok(c) => c,
        Err(_) => return None,
    };
    let parsed: AgentConfigFile = match toml::from_str(&content) {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(
                path = %toml_path.display(),
                error = %err,
                "failed to parse agent.toml",
            );
            return None;
        }
    };
    match AgentDefinition::from_config(parsed, dir.to_path_buf()) {
        Ok(def) => Some(def),
        Err(err) => {
            tracing::warn!(
                path = %toml_path.display(),
                error = %err,
                "invalid [runtime] section in agent.toml; skipping agent",
            );
            None
        }
    }
}

/// Look up an agent by name. Falls back to `_default/` when present.
pub fn load_definition_by_name(data_dir: &Path, name: &str) -> Option<AgentDefinition> {
    let root = agents_root(data_dir);
    if let Some(def) = load_definition(&root.join(name)) {
        return Some(def);
    }
    load_definition(&root.join("_default"))
}

/// Convert a definition + status into the runtime [`AgentInfo`] used by
/// REPL state, sidebar, builtins, etc.
pub fn to_agent_info(def: &AgentDefinition, status: AgentStatus) -> AgentInfo {
    let model = match &def.runtime {
        orkia_shell_types::AgentRuntimeKind::Native { model } => model.clone(),
        orkia_shell_types::AgentRuntimeKind::Vendor { command, args, .. } => {
            derive_model(command, args)
        }
    };
    AgentInfo {
        id: Uuid::new_v4(),
        name: def.name.clone(),
        archetype: def.archetype.clone(),
        status,
        model,
        dir: def.dir.clone(),
        description: def.description.clone(),
        command: def.command.clone(),
        args: def.args.clone(),
        assigned_projects: def.assigned_projects.clone(),
        max_context_tokens: def.max_context_tokens,
    }
}

/// Read `--model <id>` from args, otherwise fall back to the command
/// name so the sidebar/`ps` columns stay populated.
fn derive_model(command: &str, args: &[String]) -> String {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--model"
            && let Some(model) = it.next()
        {
            return model.clone();
        }
        if let Some(rest) = arg.strip_prefix("--model=") {
            return rest.to_string();
        }
    }
    command.to_string()
}

/// Count the number of non-blank lines in `memory.md`. Returns 0 when
/// the file does not exist.
pub fn count_memory_lines(memory_path: &Path) -> usize {
    let Ok(content) = std::fs::read_to_string(memory_path) else {
        return 0;
    };
    content.lines().filter(|l| !l.trim().is_empty()).count()
}

/// Load the tools manifest if present.
pub fn load_tools(tools_path: &Path) -> AgentToolsFile {
    let Ok(content) = std::fs::read_to_string(tools_path) else {
        return AgentToolsFile::default();
    };
    toml::from_str(&content).unwrap_or_else(|err| {
        tracing::warn!(
            path = %tools_path.display(),
            error = %err,
            "failed to parse tools.toml",
        );
        AgentToolsFile::default()
    })
}

/// Read an optional UTF-8 file. Missing → `String::new()`.
pub fn read_optional_file(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_agent(root: &Path, name: &str, body: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agent.toml"), body).unwrap();
    }

    #[test]
    fn per_agent_policy_paths_are_distinct() {
        // The per-agent `cap` model requires each agent to resolve to its own
        // policy file — `@faye` and `@rex` must never share storage.
        let data_dir = Path::new("/home/u/.orkia");
        let faye = agent_policy_path(data_dir, "faye");
        let rex = agent_policy_path(data_dir, "rex");
        assert_ne!(faye, rex);
        assert_eq!(faye, Path::new("/home/u/.orkia/agents/faye/policy.toml"));
        assert_eq!(rex, Path::new("/home/u/.orkia/agents/rex/policy.toml"));
    }

    #[test]
    fn loads_agents_in_name_order() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("agents");
        std::fs::create_dir_all(&root).unwrap();
        write_agent(
            &root,
            "sage",
            "[agent]\nname = \"sage\"\narchetype = \"qa\"\n",
        );
        write_agent(
            &root,
            "faye",
            "[agent]\nname = \"faye\"\narchetype = \"eng\"\n",
        );
        let defs = load_all_definitions(tmp.path());
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].name, "faye");
        assert_eq!(defs[1].name, "sage");
    }

    #[test]
    fn underscore_prefixed_dirs_are_skipped() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("agents");
        std::fs::create_dir_all(&root).unwrap();
        write_agent(&root, "_default", "[agent]\nname = \"_default\"\n");
        write_agent(&root, "faye", "[agent]\nname = \"faye\"\n");
        let defs = load_all_definitions(tmp.path());
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "faye");
    }

    #[test]
    fn falls_back_to_default_directory() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("agents");
        std::fs::create_dir_all(&root).unwrap();
        write_agent(&root, "_default", "[agent]\nname = \"_default\"\n");
        let def = load_definition_by_name(tmp.path(), "ghost").unwrap();
        assert_eq!(def.name, "_default");
    }

    #[test]
    fn missing_toml_is_skipped_silently() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("agents");
        std::fs::create_dir_all(root.join("broken")).unwrap();
        assert!(load_all_definitions(tmp.path()).is_empty());
    }

    #[test]
    fn derive_model_reads_flag() {
        assert_eq!(
            derive_model("claude", &["--model".into(), "sonnet".into()]),
            "sonnet",
        );
        assert_eq!(derive_model("claude", &["--model=opus".into()]), "opus",);
        assert_eq!(derive_model("codex", &[]), "codex");
    }

    #[test]
    fn count_memory_lines_skips_blank() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("memory.md");
        std::fs::write(&path, "# Memory\n\n- one\n- two\n\n").unwrap();
        assert_eq!(count_memory_lines(&path), 3);
        assert_eq!(count_memory_lines(&tmp.path().join("missing.md")), 0);
    }
}

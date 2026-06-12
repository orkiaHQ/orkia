// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

impl Repl {
    pub(crate) fn handle_agent(&mut self, args: &[String]) -> Vec<BlockContent> {
        use orkia_builtin::agent::{self, AgentAction, AgentListExtras};
        match agent::parse(args) {
            Ok(AgentAction::List) => agent::list_with_extras(&self.agents, |a| AgentListExtras {
                memory_lines: crate::agent_dir::count_memory_lines(&a.dir.join("memory.md")),
            }),
            Ok(AgentAction::Show { name }) => {
                let extras = self.show_extras(&name);
                agent::show_with_extras(&self.agents, &name, extras)
            }
            Ok(AgentAction::Create { name, archetype }) => {
                self.handle_agent_create(&name, &archetype)
            }
            Ok(AgentAction::Edit { name, file }) => self.handle_agent_edit(&name, file),
            Ok(AgentAction::Remove { name, confirm }) => self.handle_agent_remove(&name, confirm),
            Err(e) => vec![BlockContent::Error(e)],
        }
    }

    pub(crate) fn show_extras(&self, name: &str) -> orkia_builtin::agent::AgentShowExtras {
        let Some(a) = self.agents.iter().find(|a| a.name == name) else {
            return orkia_builtin::agent::AgentShowExtras::default();
        };
        let prompt = crate::agent_dir::read_optional_file(&a.dir.join("system-prompt.md"));
        let preview: Vec<String> = prompt.lines().take(10).map(String::from).collect();
        let tools = crate::agent_dir::load_tools(&a.dir.join("tools.toml"));
        orkia_builtin::agent::AgentShowExtras {
            system_prompt_preview: preview,
            memory_lines: crate::agent_dir::count_memory_lines(&a.dir.join("memory.md")),
            tools_count: tools.mcp.len() + tools.tool.len(),
        }
    }

    pub(crate) fn handle_agent_create(&mut self, name: &str, archetype: &str) -> Vec<BlockContent> {
        if !is_valid_agent_name(name) {
            return vec![BlockContent::Error(format!(
                "agent create: invalid name '{name}' (use [a-z0-9_-]+)"
            ))];
        }
        let agents_root = crate::agent_dir::agents_root(&self.config.data_dir);
        let dir = agents_root.join(name);
        if dir.exists() {
            return vec![BlockContent::Error(format!(
                "agent '{name}' already exists at {}",
                dir.display()
            ))];
        }
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return vec![BlockContent::Error(format!(
                "agent create: mkdir failed: {e}"
            ))];
        }
        let agent_toml = format!(
            "[agent]\nname = \"{name}\"\ndescription = \"\"\narchetype = \"{archetype}\"\n\n[runtime]\ncommand = \"claude\"\nargs = []\n\n[trust]\nscore = 0.70\n\n[projects]\nassigned = []\n\n[context]\nmax_context_tokens = 4000\ninclude_rfcs = true\ninclude_issues = true\n",
        );
        let prompt = orkia_builtin::agent_templates::generate_prompt_template(name, archetype);
        let today = chrono::Utc::now().format("%Y-%m-%d");
        let memory = format!("# Memory\n\n## {today}\n- Agent created\n");
        let tools = "# MCP servers and tools for this agent\n# [[mcp]]\n# name = \"github\"\n# url = \"https://mcp.github.com/sse\"\n";
        if let Err(e) = std::fs::write(dir.join("agent.toml"), &agent_toml) {
            return vec![BlockContent::Error(format!(
                "agent create: write agent.toml: {e}"
            ))];
        }
        // Propagate these like agent.toml above: a partial agent (no system
        // prompt) that still reports "created" is a silent failure (BUG-096).
        if let Err(e) = std::fs::write(dir.join("system-prompt.md"), prompt) {
            return vec![BlockContent::Error(format!(
                "agent create: write system-prompt.md: {e}"
            ))];
        }
        if let Err(e) = std::fs::write(dir.join("memory.md"), memory) {
            return vec![BlockContent::Error(format!(
                "agent create: write memory.md: {e}"
            ))];
        }
        if let Err(e) = std::fs::write(dir.join("tools.toml"), tools) {
            return vec![BlockContent::Error(format!(
                "agent create: write tools.toml: {e}"
            ))];
        }
        self.config.hydrate_agents_from_dir();
        self.agents = self.config.agents.clone();
        // Agent lifecycle (create / edit / remove) is no longer
        // sealed. It was REPL-wide noise that didn't fit the
        // scoped (job/project) chain model; bring it back as a
        // dedicated agent-scope chain when the need is concrete.
        vec![
            BlockContent::Notice {
                style: CellStyle::Good,
                text: format!("agent '{name}' created (archetype: {archetype})"),
            },
            BlockContent::Text(format!("  {}", dir.display())),
        ]
    }

    pub(crate) fn handle_agent_edit(
        &mut self,
        name: &str,
        file: orkia_builtin::agent::AgentFile,
    ) -> Vec<BlockContent> {
        let Some(agent) = self.agents.iter().find(|a| a.name == name) else {
            return vec![BlockContent::Error(format!("agent '{name}' not found"))];
        };
        if agent.dir.as_os_str().is_empty() {
            return vec![BlockContent::Error(format!(
                "agent '{name}' has no filesystem directory (legacy)"
            ))];
        }
        let path = agent.dir.join(file.filename());
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
        let status = std::process::Command::new(&editor).arg(&path).status();
        match status {
            Ok(s) if s.success() => {
                // agent.edit no longer sealed — see agent.create note.
                self.config.hydrate_agents_from_dir();
                self.agents = self.config.agents.clone();
                vec![BlockContent::SystemInfo(format!(
                    "edited {} ({})",
                    file.filename(),
                    path.display()
                ))]
            }
            Ok(s) => vec![BlockContent::Error(format!(
                "agent edit: {editor} exited with {s}"
            ))],
            Err(e) => vec![BlockContent::Error(format!(
                "agent edit: failed to launch {editor}: {e}"
            ))],
        }
    }

    pub(crate) fn handle_agent_remove(&mut self, name: &str, confirm: bool) -> Vec<BlockContent> {
        let Some(agent) = self.agents.iter().find(|a| a.name == name).cloned() else {
            return vec![BlockContent::Error(format!("agent '{name}' not found"))];
        };
        if agent.dir.as_os_str().is_empty() {
            return vec![BlockContent::Error(format!(
                "agent '{name}' has no filesystem directory (legacy)"
            ))];
        }
        if !confirm {
            return vec![
                BlockContent::SystemInfo(format!(
                    "agent remove: this will delete {}",
                    agent.dir.display()
                )),
                BlockContent::SystemInfo("re-run with --yes to confirm".into()),
            ];
        }
        if let Err(e) = std::fs::remove_dir_all(&agent.dir) {
            return vec![BlockContent::Error(format!(
                "agent remove: failed to delete {}: {e}",
                agent.dir.display()
            ))];
        }
        self.config.hydrate_agents_from_dir();
        self.agents = self.config.agents.clone();
        // agent.remove no longer sealed — see agent.create note.
        vec![BlockContent::SystemInfo(format!("agent '{name}' removed"))]
    }
}

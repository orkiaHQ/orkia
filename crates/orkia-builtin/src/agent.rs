// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use orkia_shell_types::{AgentInfo, BlockContent};

/// Sub-command produced by [`parse`]. The repl/handler is responsible
/// for any filesystem mutation (`create`, `edit`, `remove`) so this
/// crate stays I/O free.
#[derive(Debug, PartialEq)]
pub enum AgentAction {
    List,
    Show { name: String },
    Create { name: String, archetype: String },
    Edit { name: String, file: AgentFile },
    Remove { name: String, confirm: bool },
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum AgentFile {
    Prompt,
    Config,
    Memory,
    Tools,
}

impl AgentFile {
    pub fn filename(self) -> &'static str {
        match self {
            AgentFile::Prompt => "system-prompt.md",
            AgentFile::Config => "agent.toml",
            AgentFile::Memory => "memory.md",
            AgentFile::Tools => "tools.toml",
        }
    }
}

pub fn parse(args: &[String]) -> Result<AgentAction, String> {
    let mut it = args.iter();
    let sub = it.next().map(String::as_str).unwrap_or("list");
    match sub {
        "list" | "ls" => Ok(AgentAction::List),
        "show" => {
            let name = it
                .next()
                .ok_or_else(|| "agent show: missing <name>".to_string())?;
            Ok(AgentAction::Show { name: name.clone() })
        }
        "create" => parse_create(&mut it),
        "edit" => parse_edit(&mut it),
        "remove" | "rm" => parse_remove(&mut it),
        other => Err(format!("agent: unknown subcommand '{other}'")),
    }
}

fn parse_create<'a, I: Iterator<Item = &'a String>>(it: &mut I) -> Result<AgentAction, String> {
    let name = it
        .next()
        .ok_or_else(|| "agent create: missing <name>".to_string())?
        .clone();
    let mut archetype = "general".to_string();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--archetype" => {
                archetype = it
                    .next()
                    .ok_or_else(|| "agent create: --archetype needs a value".to_string())?
                    .clone();
            }
            other if other.starts_with("--archetype=") => {
                archetype = other.trim_start_matches("--archetype=").to_string();
            }
            other => {
                return Err(format!("agent create: unexpected '{other}'"));
            }
        }
    }
    Ok(AgentAction::Create { name, archetype })
}

fn parse_edit<'a, I: Iterator<Item = &'a String>>(it: &mut I) -> Result<AgentAction, String> {
    let name = it
        .next()
        .ok_or_else(|| "agent edit: missing <name>".to_string())?
        .clone();
    let mut file = AgentFile::Prompt;
    for arg in it {
        match arg.as_str() {
            "--prompt" => file = AgentFile::Prompt,
            "--config" => file = AgentFile::Config,
            "--memory" => file = AgentFile::Memory,
            "--tools" => file = AgentFile::Tools,
            other => return Err(format!("agent edit: unknown flag '{other}'")),
        }
    }
    Ok(AgentAction::Edit { name, file })
}

fn parse_remove<'a, I: Iterator<Item = &'a String>>(it: &mut I) -> Result<AgentAction, String> {
    let name = it
        .next()
        .ok_or_else(|| "agent remove: missing <name>".to_string())?
        .clone();
    let mut confirm = false;
    for arg in it {
        match arg.as_str() {
            "--yes" | "-y" | "--force" => confirm = true,
            other => return Err(format!("agent remove: unknown flag '{other}'")),
        }
    }
    Ok(AgentAction::Remove { name, confirm })
}

/// Extra columns for [`list`] derived from the agent directory (memory
/// line count, project assignments). The repl/handler computes these
/// from the filesystem and passes them paired with each agent.
#[derive(Debug, Clone, Default)]
pub struct AgentListExtras {
    pub memory_lines: usize,
}

pub fn list(agents: &[AgentInfo]) -> Vec<BlockContent> {
    list_with_extras(agents, |_| AgentListExtras::default())
}

pub fn list_with_extras<F: Fn(&AgentInfo) -> AgentListExtras>(
    agents: &[AgentInfo],
    extras: F,
) -> Vec<BlockContent> {
    if agents.is_empty() {
        return vec![
            BlockContent::SystemInfo("no agents defined".into()),
            BlockContent::SystemInfo(
                "create one: orkia agent create <name> --archetype <type>".into(),
            ),
        ];
    }
    let mut blocks = Vec::with_capacity(agents.len() + 2);
    blocks.push(BlockContent::SystemInfo(format!(
        "{} agent(s)",
        agents.len()
    )));
    // was authority over no decision. Per-(project × capability) effective trust
    // lives in the `trust` builtin.
    blocks.push(BlockContent::SystemInfo(
        " NAME         ARCHETYPE       MEMORY  PROJECTS".into(),
    ));
    for a in agents {
        let extras = extras(a);
        let projects = if a.assigned_projects.is_empty() {
            "-".to_string()
        } else {
            a.assigned_projects.join(",")
        };
        blocks.push(BlockContent::Text(format!(
            " {:<12} {:<15} {:<7} {}",
            truncate(&a.name, 12),
            truncate(&a.archetype, 15),
            extras.memory_lines,
            truncate(&projects, 40),
        )));
    }
    blocks
}

/// Extra detail for [`show`] sourced from the agent directory.
#[derive(Debug, Clone, Default)]
pub struct AgentShowExtras {
    pub system_prompt_preview: Vec<String>,
    pub memory_lines: usize,
    pub tools_count: usize,
}

pub fn show(agents: &[AgentInfo], name: &str) -> Vec<BlockContent> {
    show_with_extras(agents, name, AgentShowExtras::default())
}

pub fn show_with_extras(
    agents: &[AgentInfo],
    name: &str,
    extras: AgentShowExtras,
) -> Vec<BlockContent> {
    let Some(a) = agents.iter().find(|a| a.name == name) else {
        return vec![BlockContent::Error(format!("agent '{name}' not found"))];
    };
    let projects = if a.assigned_projects.is_empty() {
        "(none)".to_string()
    } else {
        a.assigned_projects.join(", ")
    };
    let dir_str = if a.dir.as_os_str().is_empty() {
        "(legacy)".to_string()
    } else {
        a.dir.display().to_string()
    };
    // per-(project × capability) session view (`trust @<name>`), not a scalar.
    let mut blocks = vec![
        BlockContent::SystemInfo(format!(" agent: {}", a.name)),
        BlockContent::Text(format!("   archetype  {}", a.archetype)),
        BlockContent::Text(format!("   command    {} {}", a.command, a.args.join(" "))),
        BlockContent::Text(format!("   status     {:?}", a.status)),
        BlockContent::Text(format!("   projects   {projects}")),
        BlockContent::Text(format!("   dir        {dir_str}")),
        BlockContent::Text(format!("   memory     {} line(s)", extras.memory_lines)),
        BlockContent::Text(format!("   tools      {}", extras.tools_count)),
    ];
    if let Some(desc) = &a.description {
        blocks.insert(1, BlockContent::Text(format!("   description {desc}")));
    }
    if !extras.system_prompt_preview.is_empty() {
        blocks.push(BlockContent::SystemInfo(
            "   system-prompt (first lines):".into(),
        ));
        for line in &extras.system_prompt_preview {
            blocks.push(BlockContent::Text(format!("     {line}")));
        }
    }
    blocks
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let end: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{end}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_create_with_archetype_flag() {
        let args = vec![
            "create".into(),
            "faye".into(),
            "--archetype".into(),
            "software-eng".into(),
        ];
        let parsed = parse(&args).unwrap();
        assert_eq!(
            parsed,
            AgentAction::Create {
                name: "faye".into(),
                archetype: "software-eng".into(),
            },
        );
    }

    #[test]
    fn parse_create_with_eq_archetype() {
        let args = vec!["create".into(), "faye".into(), "--archetype=devops".into()];
        let parsed = parse(&args).unwrap();
        assert_eq!(
            parsed,
            AgentAction::Create {
                name: "faye".into(),
                archetype: "devops".into(),
            },
        );
    }

    #[test]
    fn parse_create_defaults_to_general() {
        let args = vec!["create".into(), "faye".into()];
        let parsed = parse(&args).unwrap();
        assert_eq!(
            parsed,
            AgentAction::Create {
                name: "faye".into(),
                archetype: "general".into(),
            },
        );
    }

    #[test]
    fn parse_edit_files() {
        let cases = [
            (vec!["edit".into(), "faye".into()], AgentFile::Prompt),
            (
                vec!["edit".into(), "faye".into(), "--config".into()],
                AgentFile::Config,
            ),
            (
                vec!["edit".into(), "faye".into(), "--memory".into()],
                AgentFile::Memory,
            ),
            (
                vec!["edit".into(), "faye".into(), "--tools".into()],
                AgentFile::Tools,
            ),
        ];
        for (args, expected) in cases {
            let parsed = parse(&args).unwrap();
            match parsed {
                AgentAction::Edit { name, file } => {
                    assert_eq!(name, "faye");
                    assert_eq!(file, expected);
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
    }

    #[test]
    fn parse_remove_with_confirm_flag() {
        let parsed = parse(&["remove".into(), "faye".into(), "--yes".into()]).unwrap();
        assert_eq!(
            parsed,
            AgentAction::Remove {
                name: "faye".into(),
                confirm: true,
            },
        );
    }
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `orkia project ...` — list / create / show / update projects.

use orkia_shell_types::{BlockContent, Scope, Workspace};
use std::path::Path;

use crate::scope_flag::parse_scope_flag;

pub enum ProjectAction {
    List,
    Create {
        name: String,
        description: Option<String>,
        scope: Option<Scope>,
    },
    Show {
        name: String,
    },
    /// `orkia project update <name> [--description "..."] [--scope <scope>]`.
    /// PR2: introduces the variant; today only `--scope` and `--description`
    /// are recognised.
    Update {
        name: String,
        description: Option<String>,
        scope: Option<Scope>,
    },
}

/// Parse args into a project action. Args do not include the leading `project`.
pub fn parse(args: &[String]) -> Result<ProjectAction, String> {
    // Extract `--scope=<value>` (or `--scope <value>`) before the
    // subcommand-specific parser sees the args.
    let (scope, args) =
        parse_scope_flag(args).map_err(|e| format!("project: invalid --scope: {e}"))?;

    let sub = args.first().map(String::as_str).unwrap_or("list");
    match sub {
        "list" | "ls" => Ok(ProjectAction::List),
        "create" | "new" => {
            let name = args.get(1).cloned().ok_or_else(|| {
                "usage: orkia project create <name> [description] [--scope <s>]".to_string()
            })?;
            let description = if args.len() > 2 {
                Some(args[2..].join(" "))
            } else {
                None
            };
            Ok(ProjectAction::Create {
                name,
                description,
                scope,
            })
        }
        "show" => {
            let name = args
                .get(1)
                .cloned()
                .ok_or_else(|| "usage: orkia project show <name>".to_string())?;
            Ok(ProjectAction::Show { name })
        }
        "update" | "set" => {
            let name = args.get(1).cloned().ok_or_else(|| {
                "usage: orkia project update <name> [--description \"...\"] [--scope <s>]"
                    .to_string()
            })?;
            // Parse a single optional `--description "..."` flag from
            // the remaining args. The scope flag was already stripped.
            let mut description: Option<String> = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--description" | "--desc" => {
                        description = Some(
                            args.get(i + 1)
                                .cloned()
                                .ok_or_else(|| "missing value for --description".to_string())?,
                        );
                        i += 2;
                    }
                    other => return Err(format!("project update: unknown flag '{other}'")),
                }
            }
            if description.is_none() && scope.is_none() {
                return Err(
                    "project update: nothing to do; pass --scope or --description".to_string(),
                );
            }
            Ok(ProjectAction::Update {
                name,
                description,
                scope,
            })
        }
        other => Err(format!("unknown project subcommand: {other}")),
    }
}

pub fn list(workspace: &Workspace) -> Vec<BlockContent> {
    if workspace.projects.is_empty() {
        return vec![
            BlockContent::SystemInfo("no projects".into()),
            BlockContent::SystemInfo("create one: orkia project create <name>".into()),
        ];
    }
    let mut blocks = vec![BlockContent::SystemInfo(format!(
        "{} project(s)",
        workspace.projects.len()
    ))];
    for p in &workspace.projects {
        let agents = if p.assigned_agents.is_empty() {
            "none".into()
        } else {
            p.assigned_agents.join(", ")
        };
        let scope = p
            .scope
            .map(|s| format!(" [{}]", s.as_str()))
            .unwrap_or_default();
        blocks.push(BlockContent::Text(format!(
            "  {}{} — {} rfc(s), {} issue(s), agents: {}",
            p.name,
            scope,
            p.rfcs.len(),
            p.issues.len(),
            agents,
        )));
    }
    blocks
}

pub fn show(workspace: &Workspace, name: &str) -> Vec<BlockContent> {
    let Some(p) = workspace.project(name) else {
        return vec![BlockContent::Error(format!("project '{name}' not found"))];
    };
    let mut blocks = vec![
        BlockContent::SystemInfo(format!("project: {}", p.name)),
        BlockContent::Text(format!(
            "  description: {}",
            p.description.as_deref().unwrap_or("(none)")
        )),
        BlockContent::Text(format!(
            "  scope: {}",
            p.scope
                .map(|s| s.as_str().to_string())
                .unwrap_or_else(|| "(inherited)".into())
        )),
        BlockContent::Text(format!("  path: {}", p.path.display())),
        BlockContent::Text(format!(
            "  agents: {}",
            if p.assigned_agents.is_empty() {
                "none".into()
            } else {
                p.assigned_agents.join(", ")
            }
        )),
        BlockContent::Text(format!("  rfcs: {}", p.rfcs.len())),
        BlockContent::Text(format!("  issues: {}", p.issues.len())),
    ];
    for b in &p.rfcs {
        blocks.push(BlockContent::Text(format!(
            "    rfc {} [{}]: {}",
            b.slug, b.status, b.title
        )));
    }
    for i in &p.issues {
        blocks.push(BlockContent::Text(format!(
            "    #{:03} [{}/{}] {}",
            i.number, i.status, i.priority, i.title
        )));
    }
    blocks
}

pub fn create(
    workspace_root: &Path,
    name: &str,
    description: Option<&str>,
    scope: Option<Scope>,
) -> Vec<BlockContent> {
    if !workspace_root.exists()
        && let Err(e) = std::fs::create_dir_all(workspace_root)
    {
        return vec![BlockContent::Error(format!(
            "could not create projects root: {e}"
        ))];
    }
    match Workspace::create_project(workspace_root, name, description, scope) {
        Ok(path) => {
            let scope_note = scope
                .map(|s| format!(" (scope: {})", s.as_str()))
                .unwrap_or_default();
            vec![
                BlockContent::SystemInfo(format!("✓ created project '{name}'{scope_note}")),
                BlockContent::Text(format!("  {}", path.display())),
            ]
        }
        Err(e) => vec![BlockContent::Error(format!(
            "failed to create project: {e}"
        ))],
    }
}

/// Update an existing project's `description` and/or `scope`.
/// Returns the previous scope value (for SEAL emission) wrapped in the
/// blocks payload via the caller's interpretation: the REPL knows
/// which fields it asked to mutate and can re-load to inspect.
pub fn update(
    workspace: &Workspace,
    name: &str,
    description: Option<&str>,
    scope: Option<Scope>,
) -> Vec<BlockContent> {
    let Some(p) = workspace.project(name) else {
        return vec![BlockContent::Error(format!("project '{name}' not found"))];
    };
    match Workspace::update_project(&p.path, description, scope) {
        Ok(()) => {
            let mut blocks = vec![BlockContent::SystemInfo(format!(
                "✓ updated project '{name}'"
            ))];
            if description.is_some() {
                blocks.push(BlockContent::Text("  description updated".into()));
            }
            if let Some(s) = scope {
                blocks.push(BlockContent::Text(format!("  scope: {}", s.as_str())));
            }
            blocks
        }
        Err(e) => vec![BlockContent::Error(format!(
            "failed to update project: {e}"
        ))],
    }
}

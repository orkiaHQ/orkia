// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `orkia issue ...` — list / create / update issues.

use orkia_shell_types::{BlockContent, Scope, Workspace};

use crate::scope_flag::parse_scope_flag;

pub enum IssueAction {
    List {
        project: Option<String>,
    },
    Create {
        title: String,
        project: String,
        priority: String,
        scope: Option<Scope>,
    },
    Update {
        number: u32,
        project: String,
        /// Either a known scalar field (`status`, `priority`, etc.) or
        /// `"scope"`. The handler dispatches on this string.
        field: String,
        value: String,
    },
}

/// Parse args. Recognises `--project <name>`, `--priority <p>`, and `--scope <s>`.
pub fn parse(args: &[String]) -> Result<IssueAction, String> {
    // Strip the scope flag first so `split_flags` doesn't see it.
    let (scope, args) =
        parse_scope_flag(args).map_err(|e| format!("issue: invalid --scope: {e}"))?;

    let sub = args.first().map(String::as_str).unwrap_or("list");
    let rest = &args[args.len().min(1)..];
    let (positional, flags) = split_flags(rest);

    match sub {
        "list" | "ls" => Ok(IssueAction::List {
            project: flags.get("project").cloned(),
        }),
        "create" | "new" => {
            let title = positional.first().cloned().ok_or_else(|| {
                "usage: orkia issue create <title> --project <name> [--priority <p>] [--scope <s>]"
                    .to_string()
            })?;
            let project = flags
                .get("project")
                .filter(|v| !v.is_empty())
                .cloned()
                .ok_or_else(|| "--project <name> required".to_string())?;
            let priority = flags
                .get("priority")
                .cloned()
                .unwrap_or_else(|| "medium".into());
            Ok(IssueAction::Create {
                title,
                project,
                priority,
                scope,
            })
        }
        "update" => {
            let num_s = positional.first().ok_or_else(|| {
                "usage: orkia issue update <NNN> --project <name> [--status <s> | --scope <s>]"
                    .to_string()
            })?;
            let number: u32 = num_s
                .parse()
                .map_err(|_| format!("invalid issue number: {num_s}"))?;
            let project = flags
                .get("project")
                .filter(|v| !v.is_empty())
                .cloned()
                .ok_or_else(|| "--project <name> required".to_string())?;

            // `--scope` was stripped above; if it was set, this is a
            // scope-only update.
            if let Some(s) = scope {
                let other_field = flags.iter().find(|(k, _)| k.as_str() != "project");
                if other_field.is_some() {
                    return Err(
                        "issue update: --scope cannot be combined with other field flags".into(),
                    );
                }
                return Ok(IssueAction::Update {
                    number,
                    project,
                    field: "scope".into(),
                    value: s.as_str().to_string(),
                });
            }

            // Pick first scalar flag other than --project as the update field.
            let (field, value) = flags
                .iter()
                .find(|(k, _)| k.as_str() != "project")
                .map(|(k, v)| (k.clone(), v.clone()))
                .ok_or_else(|| "no field flag provided (e.g. --status done)".to_string())?;
            Ok(IssueAction::Update {
                number,
                project,
                field,
                value,
            })
        }
        other => Err(format!("unknown issue subcommand: {other}")),
    }
}

pub fn list(workspace: &Workspace, project: Option<&str>) -> Vec<BlockContent> {
    let mut blocks = Vec::new();
    let projects: Vec<_> = match project {
        Some(name) => match workspace.project(name) {
            Some(p) => vec![p],
            None => return vec![BlockContent::Error(format!("project '{name}' not found"))],
        },
        None => workspace.projects.iter().collect(),
    };

    if projects.iter().all(|p| p.issues.is_empty()) {
        blocks.push(BlockContent::SystemInfo("no issues".into()));
        return blocks;
    }

    for p in projects {
        if p.issues.is_empty() {
            continue;
        }
        blocks.push(BlockContent::SystemInfo(format!(
            "{} — {} issue(s)",
            p.name,
            p.issues.len()
        )));
        for i in &p.issues {
            let assignee = i.assigned.as_deref().unwrap_or("-");
            blocks.push(BlockContent::Text(format!(
                "  #{:03} [{}] [{}] {} ({})",
                i.number, i.status, i.priority, i.title, assignee
            )));
        }
    }
    blocks
}

pub fn create(
    workspace: &Workspace,
    project_name: &str,
    title: &str,
    priority: &str,
    scope: Option<Scope>,
) -> Vec<BlockContent> {
    let Some(p) = workspace.project(project_name) else {
        return vec![BlockContent::Error(format!(
            "project '{project_name}' not found"
        ))];
    };
    match Workspace::create_issue(&p.path, title, priority, scope) {
        Ok(path) => {
            let scope_note = scope
                .map(|s| format!(" (scope: {})", s.as_str()))
                .unwrap_or_default();
            vec![
                BlockContent::SystemInfo(format!(
                    "✓ created issue '{title}' in {project_name}{scope_note}"
                )),
                BlockContent::Text(format!("  {}", path.display())),
            ]
        }
        Err(e) => vec![BlockContent::Error(format!("failed to create issue: {e}"))],
    }
}

pub fn update(
    workspace: &Workspace,
    project_name: &str,
    number: u32,
    field: &str,
    value: &str,
) -> Vec<BlockContent> {
    let Some(p) = workspace.project(project_name) else {
        return vec![BlockContent::Error(format!(
            "project '{project_name}' not found"
        ))];
    };
    // `scope` is special-cased — Workspace::update_issue performs the
    // same `[issue].<field> = "<value>"` rewrite for it as for other
    // scalar fields, but the field's enum-ness deserves a hint when the
    // user supplies a typo'd value.
    if field == "scope"
        && let Err(e) = Scope::parse(value)
    {
        return vec![BlockContent::Error(format!("invalid scope value: {e}"))];
    }
    match Workspace::update_issue(&p.path, number, field, value) {
        Ok(_) => vec![BlockContent::SystemInfo(format!(
            "✓ updated issue #{number:03} {field}={value}"
        ))],
        Err(e) => vec![BlockContent::Error(format!("failed to update issue: {e}"))],
    }
}

// ─── helpers ────────────────────────────────────────────────────────────────

fn split_flags(args: &[String]) -> (Vec<String>, std::collections::HashMap<String, String>) {
    let mut positional = Vec::new();
    let mut flags = std::collections::HashMap::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(name) = a.strip_prefix("--") {
            let value = args.get(i + 1).cloned().unwrap_or_default();
            flags.insert(name.to_string(), value);
            i += 2;
        } else {
            positional.push(a.clone());
            i += 1;
        }
    }
    (positional, flags)
}

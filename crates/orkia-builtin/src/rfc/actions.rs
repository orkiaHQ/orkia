// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::model::RfcAction;
use super::parse::parse;
use orkia_shell_types::{BlockContent, Scope, Workspace, parse_rfc_frontmatter};
use std::path::PathBuf;

/// Legacy entry-point preserved for callers that still pass raw args.
pub fn rfc(args: &[String]) -> Vec<BlockContent> {
    match parse(args) {
        Ok(RfcAction::List { .. }) => vec![BlockContent::SystemInfo(
            "use rfc_list(workspace, project) for output".into(),
        )],
        Ok(_) => vec![BlockContent::SystemInfo(
            "rfc command dispatched by REPL".into(),
        )],
        Err(e) => vec![BlockContent::Error(e)],
    }
}

pub fn list(
    workspace: &Workspace,
    project: Option<&str>,
    status: Option<&str>,
) -> Vec<BlockContent> {
    let mut blocks = Vec::new();
    let projects: Vec<_> = match project {
        Some(name) => match workspace.project(name) {
            Some(p) => vec![p],
            None => return vec![BlockContent::Error(format!("project '{name}' not found"))],
        },
        None => workspace.projects.iter().collect(),
    };

    let filtered: Vec<(&str, Vec<&orkia_shell_types::RfcSummary>)> = projects
        .iter()
        .map(|p| {
            let rfcs: Vec<_> = p
                .rfcs
                .iter()
                .filter(|r| status.is_none_or(|s| r.status == s))
                .collect();
            (p.name.as_str(), rfcs)
        })
        .filter(|(_, rfcs)| !rfcs.is_empty())
        .collect();

    if filtered.is_empty() {
        blocks.push(BlockContent::SystemInfo("no rfcs".into()));
        return blocks;
    }

    for (name, rfcs) in filtered {
        blocks.push(BlockContent::SystemInfo(format!(
            "{name} — {} rfc(s)",
            rfcs.len()
        )));
        for b in rfcs {
            let agents = if b.assigned.is_empty() {
                "-".into()
            } else {
                b.assigned.join(",")
            };
            blocks.push(BlockContent::Text(format!(
                "  {} [{}] {} (agents: {})",
                b.slug, b.status, b.title, agents
            )));
        }
    }
    blocks
}

/// Read an RFC and render its frontmatter summary + raw body.
pub fn show(workspace: &Workspace, project_name: &str, slug: &str) -> Vec<BlockContent> {
    let path = match locate(workspace, project_name, slug) {
        Ok(p) => p,
        Err(blocks) => return blocks,
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return vec![BlockContent::Error(format!("failed to read rfc: {e}"))],
    };
    let (fm, body) = parse_rfc_frontmatter(&content);
    let header = match fm {
        Some(f) => {
            let title = f.title.unwrap_or_else(|| slug.to_string());
            let status = f.status.unwrap_or_else(|| "draft".into());
            let assigned_vec: Vec<String> = f.assigned.unwrap_or_default();
            let assigned = if assigned_vec.is_empty() {
                "-".to_string()
            } else {
                assigned_vec.join(",")
            };
            let created = f.created_at.unwrap_or_else(|| "?".into());
            format!("{title} [{status}] · assigned: {assigned} · created: {created}")
        }
        None => format!("{slug} (no frontmatter)"),
    };
    vec![
        BlockContent::SystemInfo(header),
        BlockContent::Text(body.to_string()),
    ]
}

/// Returns the rfc path (so REPL can open `$EDITOR`) on success.
///
/// PR2: `scope` is optional. When set, the file is created first via
/// `Workspace::create_rfc` (which goes through `RfcStore` for the
/// state-machine fields) and then patched with `scope = "<value>"` via
/// `Workspace::update_rfc`. Two passes keep the rfc-core API surface
/// unchanged — the alternative would be threading scope down into
/// `RfcStore::create_with_legacy`, which is in a different crate.
pub fn create(
    workspace: &Workspace,
    project_name: &str,
    title: &str,
    assigned: &[String],
    scope: Option<Scope>,
) -> Result<PathBuf, Vec<BlockContent>> {
    let Some(p) = workspace.project(project_name) else {
        return Err(vec![BlockContent::Error(format!(
            "project '{project_name}' not found"
        ))]);
    };
    let path = Workspace::create_rfc(&p.path, title, assigned)
        .map_err(|e| vec![BlockContent::Error(format!("failed to create rfc: {e}"))])?;
    if let Some(scope) = scope {
        let slug = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if let Err(e) = Workspace::update_rfc(&p.path, slug, "scope", scope.as_str()) {
            return Err(vec![BlockContent::Error(format!(
                "rfc created but scope tag failed: {e}"
            ))]);
        }
    }
    Ok(path)
}

/// Returns `(path, old_value)` on success.
pub fn update(
    workspace: &Workspace,
    project_name: &str,
    slug: &str,
    field: &str,
    value: &str,
) -> Result<(PathBuf, String), Vec<BlockContent>> {
    let Some(p) = workspace.project(project_name) else {
        return Err(vec![BlockContent::Error(format!(
            "project '{project_name}' not found"
        ))]);
    };
    if field == "scope"
        && let Err(e) = Scope::parse(value)
    {
        return Err(vec![BlockContent::Error(format!(
            "invalid scope value: {e}"
        ))]);
    }
    Workspace::update_rfc(&p.path, slug, field, value)
        .map_err(|e| vec![BlockContent::Error(format!("failed to update rfc: {e}"))])
}

pub fn locate(
    workspace: &Workspace,
    project_name: &str,
    slug: &str,
) -> Result<PathBuf, Vec<BlockContent>> {
    let Some(p) = workspace.project(project_name) else {
        return Err(vec![BlockContent::Error(format!(
            "project '{project_name}' not found"
        ))]);
    };
    p.rfcs
        .iter()
        .find(|b| b.slug == slug)
        .map(|b| b.path.clone())
        .ok_or_else(|| {
            vec![BlockContent::Error(format!(
                "rfc '{slug}' not found in {project_name}"
            ))]
        })
}

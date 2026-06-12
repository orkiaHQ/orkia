// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use std::sync::Arc;

use orkia_builtin::leave::LeaveAction;
use orkia_builtin::share::{ShareAction, UnshareKind};
use orkia_shell_types::{BlockContent, ShareIssueArgs, ShareProjectArgs, TeamClient};
use uuid::Uuid;

use crate::team_cache::TeamCache;

use super::err_block;

// ----------------------------------------------------------------------
// $share
// ----------------------------------------------------------------------

pub async fn dispatch_share(
    action: ShareAction,
    client: Arc<dyn TeamClient>,
    cache: Arc<TeamCache>,
) -> Vec<BlockContent> {
    match action {
        ShareAction::List => list_shared(cache).await,
        ShareAction::Project {
            project,
            target_workspace,
            access,
        } => {
            // Try cache lookup first (catches name → uuid resolution);
            // fall back to direct UUID parse so power users typing
            // raw IDs still work.
            let project_id = match cache.find_project(&project).await {
                Some(p) => p.id,
                None => match Uuid::parse_str(&project) {
                    Ok(u) => u,
                    Err(_) => {
                        return vec![BlockContent::Error(format!(
                            "share project: '{project}' is neither a known project name nor a UUID. Try $project list to see available projects."
                        ))];
                    }
                },
            };
            let Some(target) = Uuid::parse_str(&target_workspace).ok() else {
                return vec![BlockContent::Error(format!(
                    "share project: invalid workspace id '{target_workspace}'"
                ))];
            };
            let args = ShareProjectArgs {
                project_id,
                target_workspace_id: target,
                access,
            };
            match client.share_project(args).await {
                Ok(_) => {
                    if let Err(e) = cache.refresh().await {
                        tracing::debug!(error = ?e, "team cache refresh failed after share project");
                    }
                    vec![BlockContent::SystemInfo("\u{2713} project shared".into())]
                }
                Err(e) => err_block(&e),
            }
        }
        ShareAction::Issue {
            issue,
            target_workspace,
            access,
        } => {
            let Some(issue_id) = Uuid::parse_str(&issue).ok() else {
                return vec![BlockContent::Error(format!(
                    "share issue: invalid issue id '{issue}'"
                ))];
            };
            let Some(target) = Uuid::parse_str(&target_workspace).ok() else {
                return vec![BlockContent::Error(format!(
                    "share issue: invalid workspace id '{target_workspace}'"
                ))];
            };
            let args = ShareIssueArgs {
                issue_id,
                target_workspace_id: target,
                access,
            };
            match client.share_issue(args).await {
                Ok(_) => vec![BlockContent::SystemInfo("\u{2713} issue shared".into())],
                Err(e) => err_block(&e),
            }
        }
        ShareAction::Unshare {
            kind,
            id,
            target_workspace,
        } => {
            let Some(uuid) = Uuid::parse_str(&id).ok() else {
                return vec![BlockContent::Error(format!(
                    "share unshare: invalid id '{id}'"
                ))];
            };
            let Some(target) = Uuid::parse_str(&target_workspace).ok() else {
                return vec![BlockContent::Error(format!(
                    "share unshare: invalid workspace id '{target_workspace}'"
                ))];
            };
            match kind {
                UnshareKind::Project => match client.unshare_project(uuid, target).await {
                    Ok(_) => {
                        if let Err(e) = cache.refresh().await {
                            tracing::debug!(error = ?e, "team cache refresh failed after unshare");
                        }
                        vec![BlockContent::SystemInfo("\u{2713} project unshared".into())]
                    }
                    Err(e) => err_block(&e),
                },
                UnshareKind::Issue => vec![BlockContent::Error(
                    "share unshare issue is not yet implemented server-side (V1.5)".into(),
                )],
            }
        }
    }
}

async fn list_shared(cache: Arc<TeamCache>) -> Vec<BlockContent> {
    let data = match cache.get_or_refresh(None).await {
        Ok(d) => d,
        Err(crate::team_cache::TeamCacheError::Backend(e)) => return err_block(&e),
        Err(e) => return vec![BlockContent::Error(format!("share ls: {e}"))],
    };
    if data.shared_projects.is_empty() {
        return vec![BlockContent::SystemInfo("no shared projects".into())];
    }
    let mut blocks = vec![BlockContent::Text("PROJECT\tWORKSPACE\tACCESS".into())];
    for sp in &data.shared_projects {
        blocks.push(BlockContent::Text(format!(
            "{}\t{}\t{}",
            sp.project_id, sp.workspace_id, sp.access
        )));
    }
    blocks
}

// ----------------------------------------------------------------------
// $leave
// ----------------------------------------------------------------------

pub async fn dispatch_leave(
    action: LeaveAction,
    client: Arc<dyn TeamClient>,
    cache: Arc<TeamCache>,
) -> Vec<BlockContent> {
    if !action.confirmed {
        return vec![BlockContent::SystemInfo(
            "leave: this will remove you from the current workspace. Re-run with --yes to confirm."
                .into(),
        )];
    }
    match client.leave_workspace().await {
        Ok(true) => {
            if let Err(e) = cache.refresh().await {
                tracing::debug!(error = ?e, "team cache refresh failed after leave");
            }
            vec![BlockContent::SystemInfo(
                "\u{2713} you left the workspace".into(),
            )]
        }
        Ok(false) => vec![BlockContent::SystemInfo(
            "leave: no membership row to remove".into(),
        )],
        Err(e) => err_block(&e),
    }
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use std::path::Path;
use std::sync::Arc;

use orkia_builtin::team::TeamAction;
use orkia_shell_types::{BlockContent, CreateTeamArgs, TeamClient};
use uuid::Uuid;

use crate::session::Session;
use crate::team_cache::TeamCache;

use super::{err_block, resolve_team};

// ----------------------------------------------------------------------
// $team
// ----------------------------------------------------------------------

pub async fn dispatch_team(
    action: TeamAction,
    client: Arc<dyn TeamClient>,
    cache: Arc<TeamCache>,
    session: &mut Session,
    data_dir: &Path,
) -> Vec<BlockContent> {
    match action {
        TeamAction::List => list_teams(cache).await,
        TeamAction::Show { target } => show_team(cache, &target).await,
        TeamAction::Create {
            identifier,
            name,
            description,
            color,
        } => {
            let args = CreateTeamArgs {
                name: name.clone().unwrap_or_else(|| identifier.clone()),
                identifier: identifier.clone(),
                description,
                color,
            };
            match client.create_team(args).await {
                Ok(team) => {
                    if let Err(e) = cache.refresh().await {
                        tracing::debug!(error = ?e, "team cache refresh failed after create");
                    }
                    vec![BlockContent::SystemInfo(format!(
                        "\u{2713} created team '{}' ({})",
                        team.identifier, team.id
                    ))]
                }
                Err(e) => err_block(&e),
            }
        }
        TeamAction::Remove { target, confirmed } => {
            if !confirmed {
                return vec![BlockContent::SystemInfo(format!(
                    "team rm {target}: this will delete the team. Re-run with --yes to confirm."
                ))];
            }
            let Some(team) = cache.find_team(&target).await else {
                return vec![BlockContent::Error(format!("team not found: {target}"))];
            };
            match client.delete_team(team.id).await {
                Ok(_) => {
                    if session.current_team == Some(team.id) {
                        session.current_team = None;
                        if let Err(e) = session.save(data_dir) {
                            tracing::warn!(error = %e, "session save failed after team rm");
                        }
                    }
                    if let Err(e) = cache.refresh().await {
                        tracing::debug!(error = ?e, "team cache refresh failed after delete");
                    }
                    vec![BlockContent::SystemInfo(format!(
                        "\u{2713} removed team '{}'",
                        team.identifier
                    ))]
                }
                Err(e) => err_block(&e),
            }
        }
        TeamAction::Refresh => match cache.refresh().await {
            Ok(_) => vec![BlockContent::SystemInfo("team cache refreshed".into())],
            Err(crate::team_cache::TeamCacheError::Backend(e)) => err_block(&e),
            Err(e) => vec![BlockContent::Error(format!("team refresh failed: {e}"))],
        },
        TeamAction::Cd { target } => {
            if target == "--clear" {
                session.current_team = None;
                if let Err(e) = session.save(data_dir) {
                    return vec![BlockContent::Error(format!("session save failed: {e}"))];
                }
                return vec![BlockContent::SystemInfo("cleared current team".into())];
            }
            let Some(team) = cache.find_team(&target).await else {
                return vec![BlockContent::Error(format!("team not found: {target}"))];
            };
            session.current_team = Some(team.id);
            if let Err(e) = session.save(data_dir) {
                return vec![BlockContent::Error(format!("session save failed: {e}"))];
            }
            vec![BlockContent::SystemInfo(format!(
                "switched to team '{}'",
                team.identifier
            ))]
        }
        TeamAction::Pwd => match session.current_team {
            None => vec![BlockContent::Text("(workspace)".into())],
            Some(id) => {
                let label = cache
                    .find_team(&id.to_string())
                    .await
                    .map(|t| t.identifier)
                    .unwrap_or_else(|| id.to_string());
                vec![BlockContent::Text(label)]
            }
        },
        TeamAction::Join { nonce } => match client.join_team(&nonce).await {
            Ok(resp) => {
                // The token claims now include the new team. Refresh the
                // cache so subsequent `team ls`, `team cd` etc. see it.
                if let Err(e) = cache.refresh().await {
                    tracing::debug!(error = ?e, "team cache refresh failed after join");
                }
                let _ = data_dir; // session save not needed for join
                vec![BlockContent::SystemInfo(format!(
                    "\u{2713} joined team '{}' as {}",
                    resp.team_name, resp.role
                ))]
            }
            Err(e) => err_block(&e),
        },
    }
}

async fn list_teams(cache: Arc<TeamCache>) -> Vec<BlockContent> {
    let data = match cache.get_or_refresh(None).await {
        Ok(d) => d,
        Err(crate::team_cache::TeamCacheError::Backend(e)) => return err_block(&e),
        Err(e) => return vec![BlockContent::Error(format!("team ls: {e}"))],
    };
    if data.teams.is_empty() {
        return vec![
            BlockContent::SystemInfo("no teams in this workspace".into()),
            BlockContent::SystemInfo("create one: $team create <identifier> --name \"...\"".into()),
        ];
    }
    let mut blocks = vec![BlockContent::Text("IDENTIFIER\tNAME\tMEMBERS\tROLE".into())];
    for team in &data.teams {
        let members = data
            .team_members
            .iter()
            .filter(|m| m.team_id == team.id)
            .count();
        let my_role = data
            .team_members
            .iter()
            .find(|m| m.team_id == team.id && data.team_scope.contains(&team.id))
            .map(|m| m.role.clone())
            .unwrap_or_else(|| "-".into());
        blocks.push(BlockContent::Text(format!(
            "{}\t{}\t{}\t{}",
            team.identifier, team.name, members, my_role
        )));
    }
    blocks
}

async fn show_team(cache: Arc<TeamCache>, target: &str) -> Vec<BlockContent> {
    let Some(team) = cache.find_team(target).await else {
        return vec![BlockContent::Error(format!("team not found: {target}"))];
    };
    let data = match cache.current().await {
        Some(d) => d,
        None => return vec![BlockContent::Error("team cache empty".into())],
    };
    let members: Vec<_> = data
        .team_members
        .iter()
        .filter(|m| m.team_id == team.id)
        .collect();
    let mut blocks = vec![
        BlockContent::SystemInfo(format!("team: {}", team.identifier)),
        BlockContent::Text(format!("  name: {}", team.name)),
        BlockContent::Text(format!(
            "  description: {}",
            team.description.as_deref().unwrap_or("(none)")
        )),
        BlockContent::Text(format!(
            "  color: {}",
            team.color.as_deref().unwrap_or("(none)")
        )),
        BlockContent::Text(format!("  owner: {}", team.owner_account_id)),
        BlockContent::Text(format!("  members: {}", members.len())),
    ];
    for m in members {
        let who = m
            .account_id
            .map(|a| a.to_string())
            .or_else(|| m.agent_name.clone())
            .unwrap_or_else(|| "?".into());
        blocks.push(BlockContent::Text(format!("    {} [{}]", who, m.role)));
    }
    blocks
}

// Suppress unused — keep `resolve_team` available via re-export for future
// builtins that need to enforce workspace context explicitly.
#[allow(dead_code)]
async fn _keep_resolve_team_used(cache: &TeamCache, session: &Session) -> Option<Uuid> {
    resolve_team(cache, session, None).await
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use std::sync::Arc;

use orkia_builtin::members::MembersAction;
use orkia_shell_types::{AddMemberArgs, BlockContent, TeamClient};
use uuid::Uuid;

use crate::session::Session;
use crate::team_cache::TeamCache;

use super::{err_block, resolve_team};

// ----------------------------------------------------------------------
// $members
// ----------------------------------------------------------------------

pub async fn dispatch_members(
    action: MembersAction,
    client: Arc<dyn TeamClient>,
    cache: Arc<TeamCache>,
    session: &Session,
) -> Vec<BlockContent> {
    match action {
        MembersAction::List { team } => list_members(cache, session, team.as_deref()).await,
        MembersAction::Add {
            target,
            role,
            team,
            agent,
            project,
        } => {
            let team_id = resolve_team(cache.as_ref(), session, team.as_deref()).await;
            let project_id = project.as_deref().and_then(|p| Uuid::parse_str(p).ok());
            let account_id = Uuid::parse_str(&target).ok();
            let agent_name = if account_id.is_none() && agent.is_some() {
                agent.clone()
            } else if account_id.is_none() && !target.contains('@') {
                Some(target.clone())
            } else {
                agent
            };
            let account_id = if agent_name.is_some() {
                None
            } else if account_id.is_some() {
                account_id
            } else {
                // Fall back: treat target as email — server doesn't accept
                // emails here (it wants account_id), so we surface a clear
                // error instead of producing an opaque server 400.
                return vec![BlockContent::Error(
                    "members add: pass an account UUID or `--agent <name>`; email-based add is not supported yet".into(),
                )];
            };
            let args = AddMemberArgs {
                team_id,
                project_id,
                account_id,
                agent_name,
                role,
            };
            match client.add_team_member(args).await {
                Ok(member) => {
                    if let Err(e) = cache.refresh().await {
                        tracing::debug!(error = ?e, "team cache refresh failed after add member");
                    }
                    vec![BlockContent::SystemInfo(format!(
                        "\u{2713} added {} to team",
                        member
                            .account_id
                            .map(|a| a.to_string())
                            .or(member.agent_name)
                            .unwrap_or_else(|| "(unknown)".into())
                    ))]
                }
                Err(e) => err_block(&e),
            }
        }
        MembersAction::Remove {
            target,
            team,
            agent,
            project: _,
        } => {
            let Some(team_id) = resolve_team(cache.as_ref(), session, team.as_deref()).await else {
                return vec![BlockContent::Error(
                    "members rm: --team is required (no current team set)".into(),
                )];
            };
            let account_id = Uuid::parse_str(&target).ok();
            let agent_name = if account_id.is_none() {
                agent.or(Some(target))
            } else {
                agent
            };
            match client
                .remove_team_member(team_id, account_id, agent_name)
                .await
            {
                Ok(true) => {
                    if let Err(e) = cache.refresh().await {
                        tracing::debug!(error = ?e, "team cache refresh failed after rm member");
                    }
                    vec![BlockContent::SystemInfo("\u{2713} member removed".into())]
                }
                Ok(false) => vec![BlockContent::SystemInfo("member not found".into())],
                Err(e) => err_block(&e),
            }
        }
        MembersAction::Role {
            target,
            new_role,
            team,
        } => {
            let Some(team_id) = resolve_team(cache.as_ref(), session, team.as_deref()).await else {
                return vec![BlockContent::Error(
                    "members role: --team is required (no current team set)".into(),
                )];
            };
            let account_id = Uuid::parse_str(&target).ok();
            let agent_name = if account_id.is_none() {
                Some(target)
            } else {
                None
            };
            match client
                .change_team_member_role(team_id, account_id, agent_name, new_role.clone())
                .await
            {
                Ok(_) => {
                    if let Err(e) = cache.refresh().await {
                        tracing::debug!(error = ?e, "team cache refresh failed after role change");
                    }
                    vec![BlockContent::SystemInfo(format!(
                        "\u{2713} role updated to {new_role}"
                    ))]
                }
                Err(e) => err_block(&e),
            }
        }
    }
}

async fn list_members(
    cache: Arc<TeamCache>,
    session: &Session,
    team_flag: Option<&str>,
) -> Vec<BlockContent> {
    let data = match cache.get_or_refresh(None).await {
        Ok(d) => d,
        Err(crate::team_cache::TeamCacheError::Backend(e)) => return err_block(&e),
        Err(e) => return vec![BlockContent::Error(format!("members ls: {e}"))],
    };
    let team_id = if let Some(t) = team_flag {
        if let Ok(u) = Uuid::parse_str(t) {
            Some(u)
        } else {
            cache.find_team(t).await.map(|tm| tm.id)
        }
    } else {
        session.current_team
    };
    if let Some(tid) = team_id {
        let members: Vec<_> = data
            .team_members
            .iter()
            .filter(|m| m.team_id == tid)
            .collect();
        if members.is_empty() {
            return vec![BlockContent::SystemInfo("no team members".into())];
        }
        let mut blocks = vec![BlockContent::Text("ID\tROLE".into())];
        for m in members {
            let who = m
                .account_id
                .map(|a| a.to_string())
                .or_else(|| m.agent_name.clone())
                .unwrap_or_else(|| "?".into());
            blocks.push(BlockContent::Text(format!("{}\t{}", who, m.role)));
        }
        blocks
    } else {
        if data.workspace_members.is_empty() {
            return vec![BlockContent::SystemInfo("no workspace members".into())];
        }
        let mut blocks = vec![BlockContent::Text("ACCOUNT\tROLE".into())];
        for m in &data.workspace_members {
            blocks.push(BlockContent::Text(format!("{}\t{}", m.account_id, m.role)));
        }
        blocks
    }
}

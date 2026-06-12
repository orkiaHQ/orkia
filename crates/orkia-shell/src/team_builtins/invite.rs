// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use std::path::Path;
use std::sync::Arc;

use orkia_auth::AuthProvider;
use orkia_builtin::invite::InviteAction;
use orkia_shell_types::{BlockContent, CreateInviteArgs, TeamClient};

use crate::session::Session;
use crate::team_cache::TeamCache;

use super::err_block;

// ----------------------------------------------------------------------
// $invite
// ----------------------------------------------------------------------

/// `auth_provider` and `session` are passed alongside the client/cache
/// so the `Accept` action can adopt the returned token + flip session
/// session-persistence root.
pub async fn dispatch_invite(
    action: InviteAction,
    client: Arc<dyn TeamClient>,
    cache: Arc<TeamCache>,
    auth_provider: Option<Arc<dyn AuthProvider>>,
    session: &mut Session,
    data_dir: &Path,
) -> Vec<BlockContent> {
    match action {
        InviteAction::Create {
            email,
            role,
            ttl_days,
        } => {
            let args = CreateInviteArgs {
                email,
                role,
                ttl_days,
            };
            match client.create_invite(args).await {
                Ok(invite) => {
                    if let Err(e) = cache.refresh().await {
                        tracing::debug!(error = ?e, "team cache refresh failed after invite create");
                    }
                    vec![BlockContent::SystemInfo(format!(
                        "\u{2713} created invite: nonce={} (expires {})",
                        invite.nonce, invite.expires_at
                    ))]
                }
                Err(e) => err_block(&e),
            }
        }
        InviteAction::List { status } => {
            let data = match cache.get_or_refresh(None).await {
                Ok(d) => d,
                Err(crate::team_cache::TeamCacheError::Backend(e)) => return err_block(&e),
                Err(e) => return vec![BlockContent::Error(format!("invite ls: {e}"))],
            };
            let filtered: Vec<_> = data
                .pending_invites
                .iter()
                .filter(|inv| {
                    status
                        .as_deref()
                        .map(|s| inv.status.eq_ignore_ascii_case(s))
                        .unwrap_or(true)
                })
                .collect();
            if filtered.is_empty() {
                return vec![BlockContent::SystemInfo("no pending invites".into())];
            }
            let mut blocks = vec![BlockContent::Text("EMAIL\tROLE\tSTATUS\tEXPIRES".into())];
            for inv in filtered {
                blocks.push(BlockContent::Text(format!(
                    "{}\t{}\t{}\t{}",
                    inv.email, inv.role, inv.status, inv.expires_at
                )));
            }
            blocks
        }
        InviteAction::Revoke { nonce } => match client.revoke_invite(&nonce).await {
            Ok(_) => {
                if let Err(e) = cache.refresh().await {
                    tracing::debug!(error = ?e, "team cache refresh failed after revoke");
                }
                vec![BlockContent::SystemInfo("\u{2713} invite revoked".into())]
            }
            Err(e) => err_block(&e),
        },
        InviteAction::Accept { nonce } => match client.accept_invite(&nonce).await {
            Ok(out) => {
                let mut blocks = Vec::new();
                // flip session.current_team to None (old team scope
                // is no longer valid in the new workspace), refresh
                // the cache.
                let adopt_ok = if let Some(provider) = &auth_provider {
                    match provider.adopt_token(&out.token) {
                        Ok(()) => true,
                        Err(e) => {
                            tracing::warn!(error = ?e, "auth_provider.adopt_token failed");
                            blocks.push(BlockContent::SystemInfo(format!(
                                "(could not adopt new token automatically: {e}) \u{2014} run $logout then $login"
                            )));
                            false
                        }
                    }
                } else {
                    blocks.push(BlockContent::SystemInfo(
                        "(no auth provider wired; please re-login manually)".into(),
                    ));
                    false
                };
                if adopt_ok {
                    session.current_team = None;
                    if let Err(e) = session.save(data_dir) {
                        tracing::warn!(error = %e, "session save failed after invite accept");
                    }
                }
                // Refresh cache against the new workspace (the new
                // bearer token will route the bootstrap there).
                if let Err(e) = cache.refresh().await {
                    tracing::debug!(error = ?e, "team cache refresh failed after accept");
                }
                blocks.push(BlockContent::SystemInfo(format!(
                    "\u{2713} joined workspace {} (account {})",
                    out.workspace_id, out.account_id
                )));
                blocks
            }
            Err(e) => err_block(&e),
        },
    }
}

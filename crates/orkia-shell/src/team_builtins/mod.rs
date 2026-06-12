// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Each `dispatch_*` function takes the parsed action enum from
//! `orkia-builtin` plus the dependencies it needs (the `TeamClient`
//! trait surface, the local cache, the persistent session) and
//! returns blocks for the renderer. Kept out of `repl.rs` so the
//! main REPL file doesn't grow another six handlers.

mod invite;
mod members;
mod share_leave;
mod team;

pub use invite::dispatch_invite;
pub use members::dispatch_members;
pub use share_leave::{dispatch_leave, dispatch_share};
pub use team::dispatch_team;

use orkia_shell_types::{BlockContent, TeamClientError, team_error_message};
use uuid::Uuid;

use crate::session::Session;
use crate::team_cache::TeamCache;

pub(super) fn err_block(err: &TeamClientError) -> Vec<BlockContent> {
    vec![BlockContent::Error(team_error_message(err))]
}

#[allow(dead_code)]
pub(super) fn require_workspace(workspace_id: Option<Uuid>) -> Result<Uuid, Vec<BlockContent>> {
    workspace_id.ok_or_else(|| {
        vec![BlockContent::Error(
            "No workspace context. Switch to a workspace first.".into(),
        )]
    })
}

/// Resolve `--team <id|identifier>` or fall back to
/// `session.current_team`. Returns `None` when neither side provides
/// a team — callers either accept that (workspace-scoped operations)
/// or treat it as a usage error.
pub(super) async fn resolve_team(
    cache: &TeamCache,
    session: &Session,
    explicit: Option<&str>,
) -> Option<Uuid> {
    if let Some(t) = explicit {
        if let Ok(uuid) = Uuid::parse_str(t) {
            return Some(uuid);
        }
        if let Some(team) = cache.find_team(t).await {
            return Some(team.id);
        }
        // explicit but unresolved — caller decides how to surface
        return None;
    }
    session.current_team
}

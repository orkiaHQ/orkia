// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

impl Repl {
    pub(crate) async fn handle_team(&mut self, args: &[String]) -> Outcome {
        use orkia_builtin::team::parse;
        let action = match parse(args) {
            Ok(a) => a,
            Err(e) => {
                return Outcome::BuiltinOutput {
                    blocks: vec![BlockContent::Error(e)],
                };
            }
        };
        let touches_current_team = matches!(
            &action,
            orkia_builtin::team::TeamAction::Cd { .. }
                | orkia_builtin::team::TeamAction::Remove { .. }
        );
        let is_join = matches!(&action, orkia_builtin::team::TeamAction::Join { .. });
        let blocks = crate::team_builtins::dispatch_team(
            action,
            self.team_client.clone(),
            self.team_cache.clone(),
            &mut self.session,
            &self.config.data_dir,
        )
        .await;
        if is_join
            && let Some(BlockContent::SystemInfo(s)) = blocks.first()
            && s.contains("joined team")
        {
            // chain so the audit trail captures membership changes
            // alongside scope mutations. `current=Team` is declarative
            // — it records the kind of capability that was acquired.
            let workspace_key = self.config.data_dir.display().to_string();
            self.emit_scope_event_local(
                "workspace.team_joined",
                None,
                &workspace_key,
                None,
                orkia_shell_types::Scope::Team,
            );
        }
        self.emit_team_snapshot().await;
        if touches_current_team {
            self.emit_current_team_changed().await;
        }
        Outcome::BuiltinOutput { blocks }
    }

    pub(crate) fn handle_stream(&mut self, args: &[String]) -> Outcome {
        let action = match orkia_builtin::stream::parse(args) {
            Ok(a) => a,
            Err(e) => {
                return Outcome::BuiltinOutput {
                    blocks: vec![BlockContent::Error(e)],
                };
            }
        };
        let blocks = crate::stream_builtins::dispatch(action, self.stream_handle.as_ref());
        Outcome::BuiltinOutput { blocks }
    }

    pub(crate) async fn handle_invite(&mut self, args: &[String]) -> Outcome {
        use orkia_builtin::invite::parse;
        let action = match parse(args) {
            Ok(a) => a,
            Err(e) => {
                return Outcome::BuiltinOutput {
                    blocks: vec![BlockContent::Error(e)],
                };
            }
        };
        // Track whether `Accept` may switch session, so we can emit
        // the team-color refresh after dispatch.
        let was_accept = matches!(action, orkia_builtin::invite::InviteAction::Accept { .. });
        let blocks = crate::team_builtins::dispatch_invite(
            action,
            self.team_client.clone(),
            self.team_cache.clone(),
            self.auth_provider.clone(),
            &mut self.session,
            &self.config.data_dir,
        )
        .await;
        self.emit_team_snapshot().await;
        if was_accept {
            // Session current_team was cleared; refresh TUI color bar.
            self.emit_current_team_changed().await;
        }
        Outcome::BuiltinOutput { blocks }
    }

    pub(crate) async fn handle_members(&mut self, args: &[String]) -> Outcome {
        use orkia_builtin::members::parse;
        let action = match parse(args) {
            Ok(a) => a,
            Err(e) => {
                return Outcome::BuiltinOutput {
                    blocks: vec![BlockContent::Error(e)],
                };
            }
        };
        let blocks = crate::team_builtins::dispatch_members(
            action,
            self.team_client.clone(),
            self.team_cache.clone(),
            &self.session,
        )
        .await;
        self.emit_team_snapshot().await;
        Outcome::BuiltinOutput { blocks }
    }

    pub(crate) async fn handle_share(&mut self, args: &[String]) -> Outcome {
        use orkia_builtin::share::parse;
        let action = match parse(args) {
            Ok(a) => a,
            Err(e) => {
                return Outcome::BuiltinOutput {
                    blocks: vec![BlockContent::Error(e)],
                };
            }
        };
        let blocks = crate::team_builtins::dispatch_share(
            action,
            self.team_client.clone(),
            self.team_cache.clone(),
        )
        .await;
        self.emit_team_snapshot().await;
        Outcome::BuiltinOutput { blocks }
    }

    pub(crate) async fn handle_leave(&mut self, args: &[String]) -> Outcome {
        use orkia_builtin::leave::parse;
        let action = match parse(args) {
            Ok(a) => a,
            Err(e) => {
                return Outcome::BuiltinOutput {
                    blocks: vec![BlockContent::Error(e)],
                };
            }
        };
        let blocks = crate::team_builtins::dispatch_leave(
            action,
            self.team_client.clone(),
            self.team_cache.clone(),
        )
        .await;
        self.emit_team_snapshot().await;
        Outcome::BuiltinOutput { blocks }
    }
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

impl Repl {
    /// Emit a one-shot warning when the user declared `scope=team`
    /// without team membership. Deduped per `(artifact_id, kind)` for
    /// the lifetime of the session.
    pub(crate) fn maybe_warn_team_scope(
        &self,
        scope: Option<orkia_shell_types::Scope>,
        artifact_id: &str,
    ) -> Vec<BlockContent> {
        if scope != Some(orkia_shell_types::Scope::Team) {
            return Vec::new();
        }
        if self.team_cache.has_any_team_sync() {
            return Vec::new();
        }
        if !self
            .scope_warnings
            .should_warn(artifact_id, "team-no-membership")
        {
            return Vec::new();
        }
        vec![BlockContent::SystemInfo(
            crate::scope_warnings::messages::TEAM_NO_MEMBERSHIP.into(),
        )]
    }

    /// First-launch only: if `~/.orkiarc` is missing AND we're on a
    /// real TTY AND there's an existing rc to convert, print a prompt
    /// to stderr and read one line from stdin. On `y` / `yes`, run the
    /// migration. Anything else (including stdin closed, EOF, or
    /// piped input) skips silently. This is best-effort UX — never
    /// blocks startup if anything goes wrong.
    pub(crate) fn maybe_offer_first_run_migration(&self) {
        use std::io::{BufRead, IsTerminal, Write};

        // Only prompt interactively. Piped input must not stall here.
        if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
            return;
        }
        let Some(home) = dirs_home() else {
            return;
        };
        let orkiarc = home.join(".orkiarc");
        if orkiarc.exists() {
            return;
        }
        let Some((src, kind)) = orkia_builtin::migrate_rc::auto_detect_source(&home) else {
            return;
        };

        // Multi-line, deliberately visible. Bold + framed so the user
        // can't mistake this for the shell prompt and type a command
        // returns (yes OR no) a `.orkiarc` exists.
        {
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err);
            let _ = writeln!(
                err,
                "  \x1b[1;35m╭─ first-run setup ──────────────────────────────────────────\x1b[0m"
            );
            let _ = writeln!(
                err,
                "  \x1b[1;35m│\x1b[0m  Found \x1b[1m{}\x1b[0m \x1b[90m({})\x1b[0m.",
                src.display(),
                kind.name(),
            );
            let _ = writeln!(
                err,
                "  \x1b[1;35m│\x1b[0m  Migrate it to \x1b[1m~/.orkiarc\x1b[0m so brush picks up your aliases/exports?"
            );
            let _ = writeln!(
                err,
                "  \x1b[1;35m│\x1b[0m  \x1b[90m(answer 'no' to skip; we won't ask again — empty .orkiarc will be created)\x1b[0m"
            );
            let _ = write!(err, "  \x1b[1;35m╰─\x1b[0m  migrate now? [y/N] \x1b[?25h");
            let _ = err.flush();
        }

        let mut answer = String::new();
        let stdin = std::io::stdin();
        if stdin.lock().read_line(&mut answer).is_err() {
            return;
        }
        let answer = answer.trim().to_ascii_lowercase();
        let said_yes = answer == "y" || answer == "yes";

        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        if said_yes {
            let opts = orkia_builtin::migrate_rc::MigrateRcOpts {
                from: Some(src.clone()),
                kind: Some(kind),
                ..Default::default()
            };
            match orkia_builtin::migrate_rc::run_migration(&opts, &home, &orkiarc, &today) {
                Ok(report) => {
                    if let Some(err) = report.write_error {
                        eprintln!("  \x1b[31merror:\x1b[0m {err}");
                    } else if let Some(p) = report.written_to {
                        eprintln!("  \x1b[32m✓\x1b[0m migrated to {}", p.display());
                    }
                }
                Err(e) => eprintln!("  \x1b[31merror:\x1b[0m {e}"),
            }
        } else {
            // Decline path: write a small but USEFUL `.orkiarc` so the
            // user is not stranded without their dev tools when bashrc
            // .orkiarc exists."
            let stub = format!(
                "# ~/.orkiarc — first-run setup declined ({today}).\n\
                 # Full bash syntax. Edit freely.\n\
                 # Re-run import later with `migrate-rc --from {} --append`.\n\
                 \n\
                 # Common dev paths so cargo/brew/local binaries are findable\n\
                 # even when load_bashrc=false in config.toml.\n\
                 [ -d \"$HOME/.cargo/bin\" ]      && export PATH=\"$HOME/.cargo/bin:$PATH\"\n\
                 [ -d \"$HOME/.local/bin\" ]      && export PATH=\"$HOME/.local/bin:$PATH\"\n\
                 [ -d \"/opt/homebrew/bin\" ]     && export PATH=\"/opt/homebrew/bin:$PATH\"\n\
                 [ -d \"/opt/homebrew/sbin\" ]    && export PATH=\"/opt/homebrew/sbin:$PATH\"\n\
                 [ -d \"/usr/local/bin\" ]        && export PATH=\"/usr/local/bin:$PATH\"\n",
                src.display(),
            );
            if let Err(e) = std::fs::write(&orkiarc, stub) {
                eprintln!("  \x1b[31merror:\x1b[0m write {}: {e}", orkiarc.display());
            } else {
                eprintln!(
                    "  \x1b[90mok — wrote {} with sane PATH defaults so you won't be asked again.\x1b[0m",
                    orkiarc.display(),
                );
            }
        }
    }

    /// Drain any RC warnings the session collected at startup and emit
    /// them as `BlockContent::Error` so the user sees what broke without
    /// the shell aborting.
    pub(crate) fn surface_rc_warnings(&mut self, b: &mut BrushSession) {
        for (path, err) in b.take_rc_warnings() {
            self.emit_block(BlockContent::Error(format!("{}: {err}", path.display())));
        }
    }

    pub(crate) fn emit_workspace_snapshot(&mut self) {
        self.renderer
            .publish(RenderEvent::WorkspaceSnapshot(self.workspace.clone()));
    }

    /// renderer so widgets can redraw without their own cache
    /// subscription. Called after every team-mutating builtin.
    pub(crate) async fn emit_team_snapshot(&mut self) {
        let snapshot = self.team_cache.current().await;
        let to_send = snapshot.map(|s| orkia_shell_types::TeamSnapshot {
            workspace_id: s.workspace_id,
            seq: s.seq,
            teams: s.teams,
            team_members: s.team_members,
            workspace_members: s.workspace_members,
            pending_invites: s.pending_invites,
            shared_projects: s.shared_projects,
            projects: s.projects,
            team_scope: s.team_scope,
        });
        if let Some(snap) = to_send {
            self.renderer.publish(RenderEvent::TeamSnapshot(snap));
        }
    }

    /// Drives the team-color bar.
    pub(crate) async fn emit_current_team_changed(&mut self) {
        let team_id = self.session.current_team;
        let color = if let Some(tid) = team_id {
            self.team_cache
                .find_team(&tid.to_string())
                .await
                .and_then(|t| t.color)
        } else {
            None
        };
        self.renderer
            .publish(RenderEvent::CurrentTeamChanged { team_id, color });
    }

    pub(crate) fn emit_jobs_snapshot(&mut self) {
        self.renderer
            .publish(RenderEvent::JobsSnapshot(self.jobs.list()));
    }

    pub(crate) fn emit_migration_notice(&mut self) {
        if self.migrated_agents.is_empty() {
            return;
        }
        let names = std::mem::take(&mut self.migrated_agents);
        let msg = format!(
            "Agents migrated to ~/.orkia/agents/ ({}). Run 'orkia agent list' to see them.",
            names.join(", "),
        );
        self.renderer
            .publish(RenderEvent::Block(BlockContent::SystemInfo(msg)));
    }

    pub(crate) fn render_welcome(&mut self) {
        // With scoped chains there is no single global counter to
        // report. The shell-mode renderer task (#51) decides how
        // to surface this — for now, hand it 0 / None so the
        // existing `SEAL: N records` line displays a neutral 0.
        let info = WelcomeInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            agents: self.agents.clone(),
            seal_chain_length: 0,
            last_seal_hash: None,
        };
        self.renderer.publish(RenderEvent::Welcome(info));
    }
}

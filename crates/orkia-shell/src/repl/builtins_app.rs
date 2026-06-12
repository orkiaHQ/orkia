// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

impl Repl {
    /// `orkia app <subcommand>` — Forge app lifecycle. Run is the only
    /// variant that spawns a child (the Tauri viewer); list/edit/remove/
    /// inspect are pure filesystem operations.
    pub(crate) async fn handle_app(&mut self, args: &[String]) -> Outcome {
        use orkia_app_builtin::{AppAction, parse};
        let action = match parse(args) {
            Ok(a) => a,
            Err(e) => return Outcome::Error(e),
        };
        let root = orkia_app_builtin::default_app_root();
        let jobs = self.jobs.list();
        match action {
            AppAction::List => Outcome::BuiltinOutput {
                blocks: orkia_app_builtin::list(&root, &jobs),
            },
            AppAction::Edit { name } => match orkia_app_builtin::edit(&root, &name) {
                Ok(dir) => self.spawn_editor_on_app(&name, &dir),
                Err(blocks) => Outcome::BuiltinOutput { blocks },
            },
            AppAction::Remove { name, confirm } => {
                let confirmed = confirm.as_deref() == Some(name.as_str());
                let blocks = orkia_app_builtin::remove(&root, &name, confirm.as_deref());
                if confirmed {
                    self.emit_audit_event(
                        JobId(0),
                        "",
                        "app.remove",
                        serde_json::json!({
                            "app": name,
                            "confirmed": true,
                        }),
                    );
                }
                Outcome::BuiltinOutput { blocks }
            }
            AppAction::Inspect { name } => Outcome::BuiltinOutput {
                blocks: orkia_app_builtin::inspect(&root, &name, &jobs),
            },
            AppAction::Run { name } => self.handle_app_run(&root, &name),
            AppAction::Usage => self.handle_app_usage().await,
            AppAction::Plan => self.handle_app_plan(),
            AppAction::Perms { name } => Outcome::BuiltinOutput {
                blocks: orkia_app_builtin::perms(&root, &name),
            },
            AppAction::Seal {
                name,
                since,
                verify,
            } => Outcome::BuiltinOutput {
                blocks: orkia_app_builtin::seal(&root, &name, since.as_deref(), verify),
            },
            AppAction::Agent { name } => Outcome::BuiltinOutput {
                blocks: orkia_app_builtin::agent(&root, &name),
            },
        }
    }

    /// `orkia app usage` — fetches `/v1/forge/usage` and renders.
    pub(crate) async fn handle_app_usage(&self) -> Outcome {
        if !self.has_forge_capability() {
            return Outcome::Error(
                "Forge usage requires an Orkia premium plan. Run `$plan` to see your current tier."
                    .into(),
            );
        }
        let Some(forge) = self.forge_builder.clone() else {
            return Outcome::Error(
                "Forge is not wired in this build. Use a build with Forge enabled.".into(),
            );
        };
        match forge.usage().await {
            Ok(u) => {
                let mut blocks = Vec::new();
                blocks.push(BlockContent::Text(format!("  plan:        {}", u.plan)));
                blocks.push(BlockContent::Text(format!(
                    "  this month:  {} / {} builds",
                    u.month_used, u.month_limit
                )));
                blocks.push(BlockContent::Text(format!(
                    "  this hour:   {} / {} builds",
                    u.hour_used, u.hour_limit
                )));
                blocks.push(BlockContent::Text(format!(
                    "  reset:       {}",
                    u.reset_at.to_rfc3339()
                )));
                if !u.recent.is_empty() {
                    blocks.push(BlockContent::SystemInfo(String::new()));
                    blocks.push(BlockContent::Text("  recent builds:".into()));
                    for r in &u.recent {
                        let status = if r.success { "✓" } else { "✗" };
                        blocks.push(BlockContent::Text(format!(
                            "    {status}  {}  {} ms  {}",
                            r.created_at.format("%Y-%m-%d %H:%M"),
                            r.duration_ms,
                            r.failure_reason.as_deref().unwrap_or(""),
                        )));
                    }
                }
                Outcome::BuiltinOutput { blocks }
            }
            Err(orkia_shell_types::BuilderError::AuthRequired) => {
                Outcome::Error("not authenticated. Run: orkia login".into())
            }
            Err(e) => Outcome::Error(format!("app usage: {e}")),
        }
    }

    /// `orkia app plan` — static tier info. V1 ships free + pro shapes;
    pub(crate) fn handle_app_plan(&self) -> Outcome {
        let blocks = vec![
            BlockContent::Text("  free plan".into()),
            BlockContent::Text("    100 builds / month".into()),
            BlockContent::Text("    10 builds / hour".into()),
            BlockContent::Text("    All features available".into()),
            BlockContent::SystemInfo(String::new()),
            BlockContent::Text("  pro plan: $20/month".into()),
            BlockContent::Text("    1000 builds / month".into()),
            BlockContent::Text("    Unlimited hourly".into()),
            BlockContent::Text("    Priority queue".into()),
            BlockContent::Text("    Email support".into()),
            BlockContent::SystemInfo(String::new()),
            BlockContent::SystemInfo("upgrade at: https://orkia.dev/pricing".into()),
        ];
        Outcome::BuiltinOutput { blocks }
    }

    /// Open the app dir in `$EDITOR`. Mirrors the agent edit handler:
    /// blocking spawn so the user's editor takes over the terminal.
    pub(crate) fn spawn_editor_on_app(&mut self, name: &str, dir: &std::path::Path) -> Outcome {
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
        match std::process::Command::new(&editor).arg(dir).status() {
            Ok(s) if s.success() => Outcome::BuiltinOutput {
                blocks: vec![BlockContent::SystemInfo(format!(
                    "edited {name} in {}",
                    dir.display()
                ))],
            },
            Ok(s) => Outcome::Error(format!("{editor} exited with status {s}")),
            Err(e) => Outcome::Error(format!("failed to launch {editor}: {e}")),
        }
    }

    /// V0: spawn the placeholder viewer binary if present. Full
    /// JobController integration (so `ps`/`kill`/`fg` track the viewer
    /// process) lands together with the viewer slice — until then we
    /// spawn-and-detach and emit the SEAL `app.run` event for traceability.
    pub(crate) fn handle_app_run(&mut self, root: &std::path::Path, name: &str) -> Outcome {
        let spec = match orkia_app_builtin::prepare_run(root, name) {
            Ok(s) => s,
            Err(blocks) => return Outcome::BuiltinOutput { blocks },
        };
        let socket_path = self.config.data_dir.join("run").join("orkia.sock");
        let Some(bin) = locate_viewer_binary() else {
            self.emit_audit_event(
                JobId(0),
                "",
                "app.run",
                serde_json::json!({
                    "app": spec.app_name,
                    "pid": serde_json::Value::Null,
                    "app_dir": spec.app_dir.display().to_string(),
                    "note": "viewer binary not installed",
                }),
            );
            return Outcome::Error(
                "orkia-forge-viewer binary not found; install it before `orkia app run`".into(),
            );
        };
        let mut cmd = std::process::Command::new(&bin);
        cmd.arg("--app-dir")
            .arg(&spec.app_dir)
            .arg("--app-id")
            .arg(&spec.bundle_id)
            .arg("--socket")
            .arg(&socket_path);
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                match self.jobs.register_forge_app(spec.app_name.clone(), child) {
                    Ok(job_id) => {
                        self.emit_audit_event(
                            job_id,
                            "",
                            "app.run",
                            serde_json::json!({
                                "app": spec.app_name,
                                "pid": pid,
                                "job_id": job_id.0,
                                "app_dir": spec.app_dir.display().to_string(),
                            }),
                        );
                        Outcome::BuiltinOutput {
                            blocks: vec![
                                BlockContent::SystemInfo("spawning orkia-forge-viewer".into()),
                                BlockContent::SystemInfo(format!(
                                    "window opened · pid {pid} · job {}",
                                    job_id.0
                                )),
                            ],
                        }
                    }
                    Err(e) => Outcome::Error(format!("failed to register viewer job: {e}")),
                }
            }
            Err(e) => Outcome::Error(format!("failed to spawn viewer: {e}")),
        }
    }
}

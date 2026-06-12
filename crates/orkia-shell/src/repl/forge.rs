// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

impl Repl {
    ///
    /// When a SEAL v1 assembler is wired via `with_seal_assembler`, attempt to
    /// assemble a SEAL v1 document for the closing RFC and return a
    /// human-readable summary suffix. Failures log a warning and
    /// surface in the suffix — they never bubble up as an error,
    /// because RFC closure is a business fact and the document is an
    /// artefact that can always be regenerated via `orkia rfc seal`.
    ///
    /// When no assembler is wired (OSS build), returns `None` and the
    /// closure renders without a SEAL v1 footer message.
    pub(crate) async fn maybe_assemble_seal_v1(
        &self,
        id: &orkia_rfc_core::RfcId,
        op: &RfcTransitionOp,
    ) -> Option<String> {
        let assembler = self.seal_assembler.as_ref()?;
        let closure = match op {
            RfcTransitionOp::Abandon(reason) => orkia_shell_types::ClosureReason::Abandoned {
                reason: reason.clone(),
            },
            RfcTransitionOp::Complete => orkia_shell_types::ClosureReason::Completed,
            // Promotion / Reopen do not finalise an RFC's history.
            _ => return None,
        };
        let req = orkia_shell_types::AssembleRequest {
            rfc_id: id.clone(),
            data_dir: self.config.data_dir.clone(),
            closure,
        };
        match assembler.assemble(req).await {
            Ok(res) => Some(format!(
                " | SEAL v1 document: {} ({} events)",
                res.output_path.display(),
                res.event_count
            )),
            Err(e) => {
                tracing::warn!(
                    rfc = %id,
                    error = %e,
                    "SEAL v1 assembly failed; retry with `orkia rfc seal {}`",
                    id
                );
                Some(format!(
                    " | WARN SEAL v1 assembly failed ({e}); retry with `orkia rfc seal {id}`"
                ))
            }
        }
    }

    ///
    /// - `verify=true` and a document already exists → run verify, render outcome.
    /// - `rebuild=true` → re-assemble even if a document exists.
    /// - otherwise → assemble if missing, then display the file path.
    /// - `output=Some(path)` → copy the assembled bytes to the user-chosen path.
    pub(crate) async fn handle_rfc_seal_cli(
        &self,
        slug: String,
        verify: bool,
        rebuild: bool,
        output: Option<std::path::PathBuf>,
    ) -> Outcome {
        let Some(assembler) = self.seal_assembler.as_ref() else {
            return Outcome::BuiltinOutput {
                blocks: vec![BlockContent::SystemInfo(format!(
                    "rfc seal {slug}: this build has no SEAL v1 assembler wired. The \
                     active binary must provide on-demand SEAL v1 assembly."
                ))],
            };
        };
        let rfc_id = orkia_rfc_core::RfcId::new(slug.clone());
        let data_dir = self.config.data_dir.clone();

        // Locate the most recent existing document for this RFC, if any.
        let existing = find_latest_seal_v1_document(&data_dir, &slug);

        let doc_path = if rebuild || existing.is_none() {
            let req = orkia_shell_types::AssembleRequest {
                rfc_id,
                data_dir,
                closure: orkia_shell_types::ClosureReason::Completed,
            };
            match assembler.assemble(req).await {
                Ok(res) => res.output_path,
                Err(e) => {
                    return Outcome::Error(format!("rfc seal {slug}: assembly failed ({e})"));
                }
            }
        } else {
            // `existing` is Some here (the `if` covers `existing.is_none()`),
            // but return a typed error instead of panicking if that invariant
            // is ever broken by a refactor (BUG-080).
            match existing {
                Some(path) => path,
                None => {
                    return Outcome::Error(format!(
                        "rfc seal {slug}: internal error: existing document disappeared"
                    ));
                }
            }
        };

        if verify {
            return match assembler.verify(&doc_path).await {
                Ok(orkia_shell_types::VerifyOutcome::Valid {
                    event_count,
                    chain_head_hash,
                }) => Outcome::BuiltinOutput {
                    blocks: vec![BlockContent::SystemInfo(format!(
                        "rfc seal {slug} --verify: VALID ({event_count} events, root={chain_head_hash})"
                    ))],
                },
                Ok(orkia_shell_types::VerifyOutcome::Invalid { reason }) => {
                    Outcome::BuiltinOutput {
                        blocks: vec![BlockContent::Error(format!(
                            "rfc seal {slug} --verify: INVALID ({reason})"
                        ))],
                    }
                }
                Err(e) => Outcome::Error(format!("rfc seal {slug} --verify: {e}")),
            };
        }

        // --output: copy the document to the user-chosen path.
        if let Some(out_path) = output {
            if let Err(e) = std::fs::copy(&doc_path, &out_path) {
                return Outcome::Error(format!(
                    "rfc seal {slug} --output {}: copy failed ({e})",
                    out_path.display()
                ));
            }
            return Outcome::BuiltinOutput {
                blocks: vec![BlockContent::SystemInfo(format!(
                    "rfc seal {slug}: copied {} → {}",
                    doc_path.display(),
                    out_path.display()
                ))],
            };
        }

        Outcome::BuiltinOutput {
            blocks: vec![BlockContent::SystemInfo(format!(
                "rfc seal {slug}: {}",
                doc_path.display()
            ))],
        }
    }

    /// `orkia rfc forge <rfc-id>` — Forge V1: calls the remote
    /// builder service by default (`RemoteBuilder`); falls back to
    /// the local `ScaffoldBuilder` on `--offline`. `--rerun`
    /// regenerates an existing app while preserving `data/` and
    #[allow(clippy::too_many_arguments)] // bundled into ForgeArgs at the API boundary; this is the dispatch fan-out
    pub(crate) async fn handle_rfc_forge(
        &mut self,
        rfc_id: String,
        project: Option<String>,
        force: bool,
        offline: bool,
        rerun: bool,
        confirmed: bool,
    ) -> Outcome {
        let project_name = match self.resolve_rfc_project(project.as_deref()) {
            Ok(p) => p,
            Err(o) => return o,
        };
        let Some(p) = self.workspace.project(&project_name) else {
            return Outcome::Error(format!("project '{project_name}' not found"));
        };

        // Pick builder. `--offline` keeps the V0 scaffolder for testing
        // and demos; the default path goes to the real backend.
        if offline {
            let root = orkia_builtin::forge::default_app_root();
            return match orkia_builtin::forge::build_from_path(&p.path, &rfc_id, &root, force).await
            {
                Ok((app_dir, outcome)) => {
                    self.render_forge_success(&rfc_id, &project_name, &app_dir, &outcome, true)
                }
                Err(e) => self.render_forge_failure(&rfc_id, &project_name, e),
            };
        }

        // Remote path — capability gate first, then the injected ForgeBuilder.
        if !self.has_forge_capability() {
            self.emit_audit_event(
                JobId(0),
                "",
                "rfc.forge.failure",
                serde_json::json!({
                    "rfc_id": rfc_id, "project": project_name, "reason": "capability",
                }),
            );
            return Outcome::Error(
                "Forge build requires an Orkia premium plan. Run `$plan` to see your current tier, or use `orkia rfc forge <id> --offline` for the local scaffolder.".into(),
            );
        }
        let Some(forge) = self.forge_builder.clone() else {
            self.emit_audit_event(
                JobId(0),
                "",
                "rfc.forge.failure",
                serde_json::json!({
                    "rfc_id": rfc_id, "project": project_name, "reason": "not-wired",
                }),
            );
            return Outcome::Error(
                "Forge is not wired in this build. Use a build with Forge enabled, or run with `--offline` for the local scaffolder.".into(),
            );
        };

        // SEAL: request is going out.
        self.emit_audit_event(
            JobId(0),
            "",
            "rfc.forge.request",
            serde_json::json!({
                "rfc_id": rfc_id,
                "project": project_name,
                "rerun": rerun,
            }),
        );

        let root = orkia_forge_build::default_app_root();
        let result = orkia_forge_build::build_from_path(
            &p.path,
            &rfc_id,
            &root,
            orkia_forge_build::BuildFromPathOpts {
                force,
                rerun,
                confirmed,
            },
            forge.as_ref(),
        )
        .await;
        match result {
            Ok((app_dir, outcome)) => {
                if rerun {
                    self.emit_audit_event(
                        JobId(0),
                        "",
                        "rfc.forge.rerun",
                        serde_json::json!({
                            "rfc_id": rfc_id,
                            "project": project_name,
                            "new_hash": outcome.manifest.forge.rfc_hash,
                        }),
                    );
                }
                self.emit_audit_event(
                    JobId(0),
                    "",
                    "rfc.forge.success",
                    serde_json::json!({
                        "rfc_id": rfc_id,
                        "project": project_name,
                        "app": outcome.manifest.forge.name,
                        "rfc_hash": outcome.manifest.forge.rfc_hash,
                        "app_dir": app_dir.display().to_string(),
                        "builder_version": outcome.builder_version,
                    }),
                );
                self.render_forge_success(&rfc_id, &project_name, &app_dir, &outcome, false)
            }
            Err(e) => {
                let reason = match &e {
                    orkia_shell_types::BuilderError::AuthRequired => "auth",
                    orkia_shell_types::BuilderError::QuotaExceeded { .. } => "quota",
                    orkia_shell_types::BuilderError::RateLimit { .. } => "rate_limit",
                    orkia_shell_types::BuilderError::GenerationFailed { .. } => "validation",
                    orkia_shell_types::BuilderError::Network(_) => "network",
                    orkia_shell_types::BuilderError::ServerError => "server",
                    orkia_shell_types::BuilderError::RfcUnchanged => "unchanged",
                    _ => "other",
                };
                self.emit_audit_event(
                    JobId(0),
                    "",
                    "rfc.forge.failure",
                    serde_json::json!({
                        "rfc_id": rfc_id, "project": project_name, "reason": reason,
                    }),
                );
                self.render_forge_failure(&rfc_id, &project_name, e)
            }
        }
    }

    /// Capability gate for Forge call sites. Returns `true` when the
    /// resolver is wired and the active plan includes
    /// `Capability::ForgeBuild`. Absence of a resolver is treated as
    /// "gate open" in OSS builds where no plan is enforced — the
    /// downstream `forge_builder` (typically `NoopForgeBuilder`)
    /// surfaces the actual unavailability message.
    pub(crate) fn has_forge_capability(&self) -> bool {
        match self.capability_resolver.as_ref() {
            Some(r) => r.current().has(orkia_capabilities::Capability::ForgeBuild),
            None => true,
        }
    }

    pub(crate) fn render_forge_success(
        &self,
        rfc_id: &str,
        _project: &str,
        app_dir: &std::path::Path,
        outcome: &orkia_shell_types::BuildOutcome,
        offline: bool,
    ) -> Outcome {
        let mut blocks = Vec::new();
        blocks.push(BlockContent::SystemInfo(format!("loaded RFC {rfc_id}")));
        blocks.push(BlockContent::SystemInfo(format!(
            "RFC valid · kind: forge-app · forge.name: {}",
            outcome.manifest.forge.name
        )));
        blocks.push(BlockContent::SystemInfo(format!(
            "{} {}",
            if offline { "scaffolded" } else { "generated" },
            app_dir.display()
        )));
        for f in &outcome.files_written {
            if let Ok(rel) = f.strip_prefix(app_dir) {
                blocks.push(BlockContent::Text(format!("  {}", rel.display())));
            }
        }
        if offline {
            blocks.push(BlockContent::SystemInfo(
                "offline build · ScaffoldBuilder (placeholder app)".into(),
            ));
        } else {
            blocks.push(BlockContent::SystemInfo(format!(
                "built by {} · duration {} ms",
                outcome.builder_version,
                outcome.duration.as_millis()
            )));
        }
        blocks.push(BlockContent::SystemInfo(format!(
            "run with: orkia app run {}",
            outcome.manifest.forge.name
        )));
        Outcome::BuiltinOutput { blocks }
    }

    pub(crate) fn render_forge_failure(
        &self,
        _rfc_id: &str,
        _project: &str,
        e: orkia_shell_types::BuilderError,
    ) -> Outcome {
        use orkia_shell_types::BuilderError as BE;
        let msg = match e {
            BE::AuthRequired => {
                "not authenticated. Run: orkia login\nOr use `orkia rfc forge <id> --offline` for the local scaffolder.".into()
            }
            BE::QuotaExceeded { plan, reset_at } => format!(
                "monthly quota exceeded for {plan} plan.\nResets at: {}\nUpgrade at: https://orkia.dev/pricing",
                reset_at.to_rfc3339()
            ),
            BE::RateLimit { reset_at } => format!(
                "rate limit hit. Retry after: {}",
                reset_at.to_rfc3339()
            ),
            BE::Network(s) => format!(
                "network error: {s}\nCheck your connection, or try `orkia rfc forge <id> --offline`."
            ),
            BE::ServerError => {
                "backend returned 5xx. Check https://status.orkia.dev or try again in a moment.".into()
            }
            BE::GenerationFailed { retries, message } => format!(
                "generation failed after {retries} retries: {message}\nTry clarifying the RFC and retrying."
            ),
            BE::AppExists { name } => format!(
                "app `{name}` already exists.\nUse `orkia rfc forge <id> --rerun` to rebuild (preserves data/+seal/), or `--force` to overwrite."
            ),
            BE::RfcUnchanged => {
                "RFC unchanged since last build.\nThis rebuild would burn a quota slot for identical output.\nPass `--yes` to rebuild anyway.".into()
            }
            other => format!("rfc forge: {other}"),
        };
        Outcome::Error(msg)
    }

    /// Forge / ExitScope / Seal* variants of the `rfc` command.
    pub(crate) async fn handle_rfc_forge_seal(
        &mut self,
        action: orkia_builtin::rfc::RfcAction,
    ) -> Outcome {
        use orkia_builtin::rfc::RfcAction;
        match action {
            RfcAction::Forge {
                rfc_id,
                project,
                force,
                offline,
                rerun,
                confirmed,
            } => {
                self.handle_rfc_forge(rfc_id, project, force, offline, rerun, confirmed)
                    .await
            }
            RfcAction::ExitScope => {
                self.rfc_scope = None;
                self.rfc_scope_segment_cache = None;
                Outcome::BuiltinOutput {
                    blocks: vec![BlockContent::SystemInfo("rfc scope cleared".into())],
                }
            }
            RfcAction::Seal {
                slug,
                project: _,
                verify,
                rebuild,
                output,
            } => {
                self.handle_rfc_seal_cli(slug, verify, rebuild, output)
                    .await
            }
            RfcAction::SealExportKey { path } => Outcome::BuiltinOutput {
                blocks: vec![BlockContent::SystemInfo(format!(
                    "rfc seal --export-key {}: workspace signing-key management is a \
                     premium-tier feature. The OSS build uses \
                     ephemeral per-document signers — every document is still self-verifying.",
                    path.display()
                ))],
            },
            RfcAction::SealImportKey { path } => Outcome::BuiltinOutput {
                blocks: vec![BlockContent::SystemInfo(format!(
                    "rfc seal --import-key {}: workspace signing-key management is a \
                     premium-tier feature.",
                    path.display()
                ))],
            },
            // Safety: called only with forge/seal variants; doc/state handled above.
            _ => unreachable!("handle_rfc_forge_seal: unexpected variant"),
        }
    }
}

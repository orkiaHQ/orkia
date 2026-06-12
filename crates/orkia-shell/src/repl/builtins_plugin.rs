// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

impl Repl {
    /// installs a pre-compiled plugin into `<data_dir>/plugins/` and registers
    /// it live in the EXEC-CORE registry so it composes immediately
    /// (`ork ls | <plugin>`). No network, no compiler for the `.cwasm` path.
    pub(crate) fn handle_plugin(&mut self, args: &[String]) -> Outcome {
        match args.first().map(String::as_str).unwrap_or("list") {
            "list" => {
                let dir = crate::plugins::plugin_dir(&self.config.data_dir);
                let mut names: Vec<String> = std::fs::read_dir(&dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter_map(|e| {
                        let p = e.path();
                        if p.extension().and_then(|x| x.to_str()) == Some("cwasm") {
                            p.file_stem().and_then(|s| s.to_str()).map(String::from)
                        } else {
                            None
                        }
                    })
                    .collect();
                names.sort();
                if names.is_empty() {
                    return Outcome::BuiltinOutput {
                        blocks: vec![BlockContent::SystemInfo("no plugins installed".into())],
                    };
                }
                let mut blocks = vec![BlockContent::SystemInfo(format!(
                    "installed plugins ({}):",
                    names.len()
                ))];
                blocks.extend(names.into_iter().map(BlockContent::Text));
                Outcome::BuiltinOutput { blocks }
            }
            "add" => {
                let Some(path) = args.get(1) else {
                    return Outcome::Error("plugin add: missing <path.wasm|.cwasm|.ts|.js>".into());
                };
                let Some(runtime) = self.plugin_runtime.clone() else {
                    return Outcome::Error("plugin: wasm runtime unavailable".into());
                };
                let src = std::path::Path::new(path);
                // A directory plugin (`plugin add ./dir/`) reads its `plugin.toml`
                // NPM-V1 Volet A (multi-file). A single path keeps the V1 dispatch.
                let dest = if src.is_dir() {
                    crate::plugins::install_dir(src, &self.config.data_dir)
                } else {
                    self.install_file_plugin(src)
                };
                let dest = match dest {
                    Ok(d) => d,
                    Err(e) => return Outcome::Error(format!("plugin add: {e}")),
                };
                match crate::plugins::load_one(&dest, &runtime) {
                    Ok((name, command)) => {
                        // Live registration: clone-and-swap the registry Arc.
                        let mut reg = (*self.registry).clone();
                        reg.register(std::sync::Arc::new(command));
                        self.registry = std::sync::Arc::new(reg);
                        // capability grant for this install (audit record). The
                        // same grant is applied at every invocation (load_one).
                        let caps = match std::fs::read_to_string(dest.with_extension("toml")) {
                            Ok(text) => orkia_plugin::PluginManifest::parse(&text)
                                .map(|m| m.granted_capabilities())
                                .unwrap_or_default(),
                            Err(_) => orkia_shell_types::CapabilitySet::sandbox(),
                        };
                        let mut env = JournalEnvelope::now(EventType::Seal);
                        env.event = Some("plugin.grant".into());
                        env.source = Some(name.clone());
                        env.message = Some(if caps.is_total_sandbox() {
                            "sandbox (no effect capabilities)".to_string()
                        } else {
                            format!("granted {caps:?}")
                        });
                        self.emit_journal(env);
                        Outcome::BuiltinOutput {
                            blocks: vec![BlockContent::SystemInfo(format!(
                                "plugin `{name}` installed and registered"
                            ))],
                        }
                    }
                    Err(e) => Outcome::Error(format!("plugin add: {e}")),
                }
            }
            "dev" => self.handle_plugin_dev(args),
            other => Outcome::Error(format!(
                "plugin: unknown subcommand `{other}` (use `add <path>`, `dev <path>`, or `list`)"
            )),
        }
    }

    /// Install a single-file plugin by extension: `.cwasm` directly; `.ts`/`.js`
    /// and raw `.wasm` via the pulled `orkia-compiler` (the runtime-only binary
    /// has no compiler). Returns the installed `.cwasm` path.
    pub(crate) fn install_file_plugin(
        &self,
        src: &std::path::Path,
    ) -> Result<std::path::PathBuf, orkia_plugin::PluginError> {
        let ext = src.extension().and_then(|e| e.to_str()).unwrap_or_default();
        let name = src
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin")
            .to_string();
        let manifest = src.with_extension("toml");
        match ext {
            "cwasm" => {
                crate::plugins::install_cwasm(src, Some(&manifest), &name, &self.config.data_dir)
            }
            "ts" | "tsx" | "mts" | "js" | "mjs" | "wasm" => {
                let cwasm = crate::plugins::compile_source(src)?;
                crate::plugins::install_cwasm(&cwasm, Some(&manifest), &name, &self.config.data_dir)
            }
            other => Err(orkia_plugin::PluginError::Load(format!(
                "unsupported `.{other}` (use a directory, .wasm, .cwasm, .ts, or .js)"
            ))),
        }
    }

    /// register once, then watch the source and live-reload on every save. Dev
    /// mode only — distinct from `add`, which installs once and does not watch.
    pub(crate) fn handle_plugin_dev(&mut self, args: &[String]) -> Outcome {
        let Some(path) = args.get(1) else {
            return Outcome::Error("plugin dev: missing <path.ts|.js>".into());
        };
        let Some(runtime) = self.plugin_runtime.clone() else {
            return Outcome::Error("plugin: wasm runtime unavailable".into());
        };
        let src = std::path::Path::new(path).to_path_buf();
        let name = src
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin")
            .to_string();

        // Initial compile + install + register (same as `add`).
        let dest = match crate::plugins::compile_source(&src) {
            Ok(cwasm) => crate::plugins::install_cwasm(
                &cwasm,
                Some(&src.with_extension("toml")),
                &name,
                &self.config.data_dir,
            ),
            Err(e) => return Outcome::Error(format!("plugin dev: {e}")),
        };
        let dest = match dest {
            Ok(d) => d,
            Err(e) => return Outcome::Error(format!("plugin dev: {e}")),
        };
        let reg_name = match crate::plugins::load_one(&dest, &runtime) {
            Ok((reg_name, command)) => {
                let mut reg = (*self.registry).clone();
                reg.register(std::sync::Arc::new(command));
                self.registry = std::sync::Arc::new(reg);
                reg_name
            }
            Err(e) => return Outcome::Error(format!("plugin dev: {e}")),
        };

        // Watch for subsequent saves (recompile + live-reload off the REPL).
        if let Err(e) = crate::plugins::spawn_dev_watcher(
            src,
            name,
            runtime,
            self.config.data_dir.clone(),
            self.plugin_dev_tx.clone(),
        ) {
            return Outcome::Error(format!("plugin dev: {e}"));
        }
        Outcome::BuiltinOutput {
            blocks: vec![BlockContent::SystemInfo(format!(
                "plugin `{reg_name}` registered; watching `{path}` for changes (dev mode)"
            ))],
        }
    }
}

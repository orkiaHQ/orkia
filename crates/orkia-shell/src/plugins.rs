// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Installing TS/WASM plugins into the EXEC-CORE registry —
//!
//! An installed plugin is a pre-compiled `.cwasm` under `<data_dir>/plugins/`
//! with an optional `<name>.toml` manifest. At startup every installed plugin
//! is loaded and registered as a `Command`, so it composes in pipelines
//! (`ork ls | where_big --min_size 1mb`). The runtime is sandboxed and
//! fail-closed; a plugin that fails to load is skipped, the
//! shell still starts.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use orkia_plugin::runtime::PluginMeta;
use orkia_plugin::{PluginCommand, PluginError, PluginManifest, PluginRuntime};

use crate::exec::registry::CommandRegistry;

/// The per-session plugin directory: `<data_dir>/plugins`.
pub fn plugin_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("plugins")
}

/// Load every `*.cwasm` in `dir` and register it. Best-effort: a failing
/// plugin is logged and skipped. Returns the names registered.
pub fn load_all(
    dir: &Path,
    registry: &mut CommandRegistry,
    runtime: &Arc<PluginRuntime>,
) -> Vec<String> {
    let mut loaded = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return loaded;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("cwasm") {
            continue;
        }
        match load_one(&path, runtime) {
            Ok((name, command)) => {
                registry.register(Arc::new(command));
                loaded.push(name);
            }
            Err(e) => tracing::warn!(plugin = %path.display(), error = %e, "plugin load failed"),
        }
    }
    loaded.sort();
    loaded
}

/// Load a single `.cwasm` (+ optional sidecar `.toml` manifest) into a command.
pub fn load_one(
    path: &Path,
    runtime: &Arc<PluginRuntime>,
) -> Result<(String, PluginCommand), PluginError> {
    let bytes = std::fs::read(path).map_err(|e| PluginError::Load(e.to_string()))?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("plugin")
        .to_string();
    let manifest = match std::fs::read_to_string(path.with_extension("toml")) {
        Ok(text) => PluginManifest::parse(&text)?,
        Err(_) => PluginManifest::sandbox_default(&stem),
    };

    let signature = manifest.to_signature()?;
    let name = manifest.plugin.name.clone();
    let meta = PluginMeta {
        name: name.clone(),
        version: manifest.plugin.version.clone(),
        description: manifest.plugin.description.clone().unwrap_or_default(),
        streaming: manifest.command.streaming,
        signature,
    };
    let plugin = Arc::new(runtime.load_precompiled(meta, &bytes)?);

    // The unified `CapabilitySet` granted to this plugin, parsed from its
    // (user-approved) manifest. Empty manifest ⇒ total sandbox. Effects still
    let caps = manifest.granted_capabilities();
    Ok((name, PluginCommand::new(plugin, runtime.clone(), caps)))
}

/// Install a pre-compiled `.cwasm` (+ optional manifest) into the plugin dir as
/// `<name>.cwasm` (+ `<name>.toml`), returning the destination path.
pub fn install_cwasm(
    cwasm: &Path,
    manifest_src: Option<&Path>,
    name: &str,
    data_dir: &Path,
) -> Result<PathBuf, PluginError> {
    let dir = plugin_dir(data_dir);
    std::fs::create_dir_all(&dir).map_err(|e| PluginError::Load(e.to_string()))?;
    let dest = dir.join(format!("{name}.cwasm"));
    std::fs::copy(cwasm, &dest).map_err(|e| PluginError::Load(e.to_string()))?;
    if let Some(m) = manifest_src
        && m.exists()
    {
        let _ = std::fs::copy(m, dir.join(format!("{name}.toml")));
    }
    Ok(dest)
}

/// Locate the `orkia-compiler` artifact: `$ORKIA_COMPILER`, then
/// `~/.orkia/cache/compiler/orkia-compiler`, then `PATH`.
pub fn find_compiler() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("ORKIA_COMPILER") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let cached = PathBuf::from(home).join(".orkia/cache/compiler/orkia-compiler");
        if cached.is_file() {
            return Some(cached);
        }
    }
    // Do not fall back to an unqualified PATH lookup — a malicious or
    // shadowed `orkia-compiler` on $PATH could execute arbitrary code.
    // Return None so the caller surfaces "compiler not found, run
    // `orkia compiler install`" instead.
    None
}

/// Compile a `.ts`/`.js` source — or AOT-precompile a raw `.wasm` (any source
/// the `orkia-compiler` artifact (pulled on demand; the default binary
/// never links the compiler). Returns the temp `.cwasm` path.
pub fn compile_source(src: &Path) -> Result<PathBuf, PluginError> {
    let compiler = find_compiler().ok_or_else(|| {
        PluginError::Load(
            "plugin compiler not found — run `orkia compiler install` (needs network) \
             or set $ORKIA_COMPILER"
                .to_string(),
        )
    })?;
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("plugin");
    let out = std::env::temp_dir().join(format!("orkia-plugin-{stem}.cwasm"));
    let output = std::process::Command::new(&compiler)
        .arg("compile")
        .arg(src)
        .arg("-o")
        .arg(&out)
        .output()
        .map_err(|e| PluginError::Load(format!("invoke compiler `{}`: {e}", compiler.display())))?;
    if !output.status.success() {
        return Err(PluginError::Load(format!(
            "compile failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(out)
}

/// `<dir>/plugin.toml` for its `entry`, bundle that entry's local module graph
/// (multi-file + pure-JS `node_modules`) via the compiler, and install under the
/// manifest's plugin name. The bundling lives entirely in `orkia-compiler`
/// (pulled, off-binary); the shell only resolves the entry and shells out.
pub fn install_dir(dir: &Path, data_dir: &Path) -> Result<PathBuf, PluginError> {
    let manifest_path = dir.join("plugin.toml");
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| PluginError::Load(format!("read {}: {e}", manifest_path.display())))?;
    let manifest = PluginManifest::parse(&text)?;
    let entry = manifest.plugin.entry.as_deref().ok_or_else(|| {
        PluginError::Load("directory plugin needs `entry` in [plugin] of plugin.toml".to_string())
    })?;
    let cwasm = compile_source(&dir.join(entry))?;
    install_cwasm(
        &cwasm,
        Some(&manifest_path),
        &manifest.plugin.name,
        data_dir,
    )
}

/// A `plugin dev` watcher's message to the REPL: a freshly-recompiled command
/// to swap into the registry, or a recompile failure to surface.
pub enum DevReloadMsg {
    Reloaded {
        name: String,
        command: PluginCommand,
    },
    Failed {
        name: String,
        error: String,
    },
}

/// Recompile a dev plugin source and rebuild its command: compile → install
/// (overwrite `<name>.cwasm`) → load. Runs on the watcher thread, off the REPL.
fn recompile(
    src: &Path,
    name: &str,
    runtime: &Arc<PluginRuntime>,
    data_dir: &Path,
) -> Result<(String, PluginCommand), PluginError> {
    let cwasm = compile_source(src)?;
    let dest = install_cwasm(&cwasm, Some(&src.with_extension("toml")), name, data_dir)?;
    load_one(&dest, runtime)
}

/// Spawn a filesystem watcher that recompiles + re-registers a plugin whenever
/// its source changes — `plugin dev`. All the slow work
/// (compile subprocess, deserialize) happens on this dedicated thread; the REPL
/// only does the registry Arc-swap when it drains the channel, so the loop never
/// blocks on compilation. The watcher is owned by the thread (one owner) and
/// lives for the session. Returns once watching is established (or errors if the
/// OS watcher can't be created).
pub fn spawn_dev_watcher(
    src: PathBuf,
    name: String,
    runtime: Arc<PluginRuntime>,
    data_dir: PathBuf,
    tx: std::sync::mpsc::Sender<DevReloadMsg>,
) -> Result<(), PluginError> {
    use notify::{RecursiveMode, Watcher};

    let (raw_tx, raw_rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = raw_tx.send(res);
    })
    .map_err(|e| PluginError::Load(format!("dev watcher init: {e}")))?;

    // Watch the parent directory (editors save via atomic rename, which a
    // single-file watch can miss); filter events down to our source file.
    let watch_dir = src
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    watcher
        .watch(&watch_dir, RecursiveMode::NonRecursive)
        .map_err(|e| PluginError::Load(format!("watch {}: {e}", watch_dir.display())))?;

    std::thread::spawn(move || {
        // Keep the watcher alive for the thread's lifetime (drop ⇒ stop watching).
        let _watcher = watcher;
        let file_name = src.file_name().map(|s| s.to_os_string());
        // Loops until the watcher (and its sender) is dropped.
        while let Ok(first) = raw_rx.recv() {
            // Coalesce the burst of events a single save emits.
            while let Ok(_extra) = raw_rx.try_recv() {}
            let Ok(event) = first else { continue };
            let touched = event
                .paths
                .iter()
                .any(|p| p == &src || p.file_name().map(|n| n.to_os_string()) == file_name);
            if !touched {
                continue;
            }
            let msg = match recompile(&src, &name, &runtime, &data_dir) {
                Ok((reg_name, command)) => DevReloadMsg::Reloaded {
                    name: reg_name,
                    command,
                },
                Err(e) => DevReloadMsg::Failed {
                    name: name.clone(),
                    error: e.to_string(),
                },
            };
            if tx.send(msg).is_err() {
                break; // REPL gone
            }
        }
    });
    Ok(())
}

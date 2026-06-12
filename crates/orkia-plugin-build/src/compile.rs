// Copyright 2026 Orkia
// SPDX-License-Identifier: Apache-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Apache License 2.0; see https://www.apache.org/licenses/LICENSE-2.0
// for terms.

//!
//! `source.ts` → (OXC) JS → (Javy) QuickJS-WASM → (wasmtime, Cranelift) AOT
//! `.cwasm`. The resulting `.cwasm` is loaded by the runtime-only `orkia`
//! binary via `Module::deserialize` — so the engine config used to precompile
//! here MUST match the runtime's (`consume_fuel(true)`).

use std::path::Path;
use std::process::Command;

use crate::error::CompileError;
use crate::pull::ensure_javy;

/// Compile a plugin into a precompiled `.cwasm`. `.ts`/`.js` go through the
/// OXC→Javy→Cranelift pipeline; a raw `.wasm` (any source language — e.g. a
/// already WASM, so it skips transpile+Javy and is only AOT-precompiled. Either
/// way the precompile uses the runtime-matching engine config — the runtime-only
/// `orkia` binary has no compiler, so this artifact is the single place a
/// `.wasm` becomes a loadable `.cwasm`.
pub fn compile_file(entry: &Path) -> Result<Vec<u8>, CompileError> {
    let ext = entry
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    // Raw WASM: precompile only, no source read (the bytes aren't UTF-8).
    if ext == "wasm" {
        let wasm = std::fs::read(entry).map_err(|e| CompileError::Io(e.to_string()))?;
        return precompile(&wasm);
    }
    // TS/JS source: bundle the local module graph from this entry (multi-file,
    // a fast path that is byte-identical to the V1 single-file output.
    let js = match ext {
        "ts" | "tsx" | "mts" | "js" | "mjs" | "cjs" => crate::bundle::bundle_entry(entry, ext)?,
        other => {
            return Err(CompileError::Transpile(format!(
                "unsupported source extension `.{other}` (expected .ts/.js/.wasm)"
            )));
        }
    };
    let wasm = javy_compile(&js)?;
    precompile(&wasm)
}

/// JS → QuickJS-WASM via the Javy compiler (pulled + verified on demand).
fn javy_compile(js: &str) -> Result<Vec<u8>, CompileError> {
    let javy = ensure_javy()?;
    let dir = tempfile::tempdir().map_err(|e| CompileError::Io(e.to_string()))?;
    let in_js = dir.path().join("plugin.js");
    let out_wasm = dir.path().join("plugin.wasm");
    std::fs::write(&in_js, js).map_err(|e| CompileError::Io(e.to_string()))?;

    let output = Command::new(&javy)
        .arg("build")
        .arg(&in_js)
        .arg("-o")
        .arg(&out_wasm)
        .output()
        .map_err(|e| CompileError::Javy(format!("spawn javy: {e}")))?;
    if !output.status.success() {
        return Err(CompileError::Javy(format!(
            "javy build failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    std::fs::read(&out_wasm).map_err(|e| CompileError::Io(e.to_string()))
}

/// AOT-precompile the WASM for wasmtime. The config must match the runtime's
/// `PluginRuntime` (fuel enabled) or `Module::deserialize` rejects it.
fn precompile(wasm: &[u8]) -> Result<Vec<u8>, CompileError> {
    let mut config = wasmtime::Config::new();
    config.consume_fuel(true);
    let engine =
        wasmtime::Engine::new(&config).map_err(|e| CompileError::Precompile(e.to_string()))?;
    engine
        .precompile_module(wasm)
        .map_err(|e| CompileError::Precompile(e.to_string()))
}

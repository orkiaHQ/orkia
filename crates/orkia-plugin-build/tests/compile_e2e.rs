// Copyright 2026 Orkia
// SPDX-License-Identifier: Apache-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Apache License 2.0; see https://www.apache.org/licenses/LICENSE-2.0
// for terms.

//!
//! Compiles a **TypeScript** plugin source (OXC strips types) → JS → Javy
//! (QuickJS-WASM) → wasmtime AOT `.cwasm`, then runs it through the real
//! runtime. Requires the Javy compiler (resolved by `ensure_javy`: `$ORKIA_JAVY`
//! → `~/.orkia/cache/compiler/javy` → download). If unavailable offline, the
//! test skips rather than failing.

use orkia_plugin::CapabilitySet;
use orkia_plugin::PluginManifest;
use orkia_plugin::runtime::{PluginMeta, PluginRuntime};
use orkia_plugin_build::{CompileError, compile_file};

const PILOT_TS: &str = r#"
// A TypeScript plugin: reads the {input, call} envelope and doubles input.n.
function readAll(): string {
  const chunks: Uint8Array[] = [];
  const buf = new Uint8Array(1024);
  let n: number;
  while ((n = (Javy as any).IO.readSync(0, buf)) > 0) { chunks.push(buf.slice(0, n)); }
  let len: number = 0;
  for (const c of chunks) len += c.length;
  const all = new Uint8Array(len);
  let o = 0;
  for (const c of chunks) { all.set(c, o); o += c.length; }
  return new TextDecoder().decode(all);
}
const env: any = JSON.parse(readAll());
const value: number = (env.input && env.input.n) || 0;
const out: string = JSON.stringify({ doubled: value * 2 });
(Javy as any).IO.writeSync(1, new TextEncoder().encode(out));
"#;

#[test]
fn compile_typescript_and_run() {
    let dir = tempfile::tempdir().expect("tmp");
    let ts = dir.path().join("double.ts");
    std::fs::write(&ts, PILOT_TS).expect("write ts");

    // TS → JS (OXC) → WASM (Javy) → .cwasm (wasmtime AOT).
    let cwasm = match compile_file(&ts) {
        Ok(bytes) => bytes,
        Err(CompileError::Pull(reason)) => {
            eprintln!("skip: Javy compiler unavailable offline ({reason})");
            return;
        }
        Err(e) => panic!("compile failed: {e}"),
    };
    assert!(!cwasm.is_empty(), "produced a .cwasm");

    // Run the compiled plugin through the runtime.
    let runtime = PluginRuntime::new().expect("runtime");
    let signature = PluginManifest::sandbox_default("double")
        .to_signature()
        .expect("sig");
    let plugin = runtime.load_precompiled(
        PluginMeta {
            name: "double".to_string(),
            version: "0.1.0".to_string(),
            description: String::new(),
            streaming: false,
            signature,
        },
        &cwasm,
    );
    let plugin = plugin.expect("load compiled .cwasm");

    let out = runtime
        .run_wasi_json(
            &plugin,
            &CapabilitySet::sandbox(),
            r#"{"input":{"n":21},"call":{"positional":[],"named":{}}}"#,
        )
        .expect("run compiled plugin")
        .output;
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(parsed["doubled"], 42, "compiled TS plugin doubled 21 → 42");
}

/// importing `./mathlib.ts`) bundles into one module and runs. Proves the
/// resolver + ESM→CJS bundle actually executes under QuickJS, not just that the
/// emitted JS looks right.
const MAIN_TS: &str = r#"
import { triple } from "./mathlib";
function readAll(): string {
  const chunks: Uint8Array[] = [];
  const buf = new Uint8Array(1024);
  let n: number;
  while ((n = (Javy as any).IO.readSync(0, buf)) > 0) { chunks.push(buf.slice(0, n)); }
  let len: number = 0;
  for (const c of chunks) len += c.length;
  const all = new Uint8Array(len);
  let o = 0;
  for (const c of chunks) { all.set(c, o); o += c.length; }
  return new TextDecoder().decode(all);
}
const env: any = JSON.parse(readAll());
const value: number = (env.input && env.input.n) || 0;
const out: string = JSON.stringify({ tripled: triple(value) });
(Javy as any).IO.writeSync(1, new TextEncoder().encode(out));
"#;

const MATHLIB_TS: &str = r#"
export function triple(n: number): number { return n * 3; }
"#;

#[test]
fn compile_multifile_bundle_and_run() {
    let dir = tempfile::tempdir().expect("tmp");
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).expect("mkdir");
    std::fs::write(src.join("mathlib.ts"), MATHLIB_TS).expect("write mathlib");
    let main = src.join("main.ts");
    std::fs::write(&main, MAIN_TS).expect("write main");

    let cwasm = match compile_file(&main) {
        Ok(bytes) => bytes,
        Err(CompileError::Pull(reason)) => {
            eprintln!("skip: Javy compiler unavailable offline ({reason})");
            return;
        }
        Err(e) => panic!("multi-file compile failed: {e}"),
    };
    assert!(
        !cwasm.is_empty(),
        "produced a .cwasm from a multi-file plugin"
    );

    let runtime = PluginRuntime::new().expect("runtime");
    let signature = PluginManifest::sandbox_default("main")
        .to_signature()
        .expect("sig");
    let plugin = runtime
        .load_precompiled(
            PluginMeta {
                name: "main".to_string(),
                version: "0.1.0".to_string(),
                description: String::new(),
                streaming: false,
                signature,
            },
            &cwasm,
        )
        .expect("load compiled .cwasm");

    let out = runtime
        .run_wasi_json(
            &plugin,
            &CapabilitySet::sandbox(),
            r#"{"input":{"n":7},"call":{"positional":[],"named":{}}}"#,
        )
        .expect("run bundled plugin")
        .output;
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(
        parsed["tripled"], 21,
        "bundled multi-file plugin: triple(7) = 21"
    );
}

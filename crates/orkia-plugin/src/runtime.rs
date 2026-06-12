// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! wasmtime configured **runtime-only** (no Cranelift) — the default binary
//! ships only the executor; modules are pre-compiled AOT (`.cwasm`) by the
//! separate compiler artifact and loaded via [`wasmtime::Module::deserialize`].
//! Resource limits (fuel + memory) are enforced so a runaway guest is stopped
//! by wasmtime without crashing the host.
//!
//! ## Guest ABI (host ↔ module data path)
//!
//! A module executed here MUST export:
//! - `memory` — its linear memory;
//! - `alloc(len: i32) -> i32` — reserve `len` bytes, return a pointer;
//! - `run(ptr: i32, len: i32) -> i64` — read `len` JSON input bytes at `ptr`,
//!   compute, write JSON output into memory, and return a packed pointer/length
//!   `(out_ptr as i64) << 32 | (out_len as i64)`.
//!
//! The QuickJS-WASM module (built by `orkia-plugin-build`) conforms to this
//! ABI; the JSON it reads/writes is the `Value` boundary form. Any
//! ABI-conforming module works — that is the polyglot seam.

use orkia_shell_types::{CapabilitySet, Signature};
use wasmtime::{Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};

use crate::error::PluginError;

/// Default fuel budget per plugin invocation (wasmtime "fuel" ≈ executed wasm
/// ops). Generous for a transform, bounded so an infinite loop is stopped.
const DEFAULT_FUEL: u64 = 1_000_000_000;
/// Fuel for a WASI/QuickJS guest — QuickJS engine init alone is many millions
/// of ops, so this is larger, still finite (caps a runaway).
const WASI_FUEL: u64 = 20_000_000_000;
/// Default linear-memory ceiling per invocation (64 MiB).
const DEFAULT_MAX_MEMORY: usize = 64 * 1024 * 1024;

/// Per-`Store` state for the memory-ABI path. Holds the resource limits.
pub struct StoreState {
    limits: StoreLimits,
}

impl StoreState {
    fn new(max_memory: usize) -> Self {
        Self {
            limits: StoreLimitsBuilder::new().memory_size(max_memory).build(),
        }
    }
}

/// Per-`Store` state for the WASI path: resource limits + the sandboxed WASI
/// preview1 context (stdio only).
struct WasiState {
    limits: StoreLimits,
    wasi: wasmtime_wasi::p1::WasiP1Ctx,
}

/// The result of a WASI guest run: its stdout (the JSON output that crosses the
/// pipe — the only content that does) and any stderr it wrote. A QuickJS-WASM
/// guest sends `console.log`/`console.error` to stderr (Javy's default), and a
/// Rust SDK guest writes diagnostics there too; the host routes that to the
/// journal so a plugin can never corrupt the pipe with logging.
#[derive(Debug)]
pub struct WasiRun {
    pub output: String,
    pub logs: String,
}

/// A loaded, ready-to-run plugin.
pub struct LoadedPlugin {
    pub name: String,
    pub version: String,
    pub description: String,
    pub streaming: bool,
    pub signature: Signature,
    pub(crate) module: Module,
}

/// The plugin execution runtime — one per shell session.
pub struct PluginRuntime {
    engine: Engine,
    max_memory: usize,
    fuel: u64,
    wasi_fuel: u64,
}

impl PluginRuntime {
    /// Build a runtime-only engine with fuel metering enabled.
    pub fn new() -> Result<Self, PluginError> {
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine =
            Engine::new(&config).map_err(|e| PluginError::Load(format!("engine init: {e}")))?;
        Ok(Self {
            engine,
            max_memory: DEFAULT_MAX_MEMORY,
            fuel: DEFAULT_FUEL,
            wasi_fuel: WASI_FUEL,
        })
    }

    /// Override the per-invocation fuel budget for the WASI/QuickJS path.
    /// (Tests use a smaller budget to exercise the runaway-stop path quickly;
    /// the default is generous enough for QuickJS engine init.)
    pub fn with_wasi_fuel(mut self, fuel: u64) -> Self {
        self.wasi_fuel = fuel;
        self
    }

    /// The underlying engine (for precompilation in the compiler / tests).
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Load a pre-compiled `.cwasm` (AOT) module. Fail-closed: an
    /// undeserializable or incompatible blob is rejected, never run.
    ///
    /// # Safety
    /// `wasmtime::Module::deserialize` is **unsafe** and provides no memory-
    /// safety guarantee against adversarially crafted bytes. A wasmtime version
    /// or config mismatch may be caught as a load error, but a forged or
    /// corrupted `.cwasm` blob can cause **undefined behaviour** (memory
    /// corruption, arbitrary code execution in the host process) — *not* a
    /// clean error. The caller is responsible for verifying the blob's integrity
    /// (hash, signature) **before** calling this function. For a fail-closed
    /// alternative that enforces a SHA-256 check inside this crate, use
    /// [`load_verified`][Self::load_verified].
    pub fn load_precompiled(
        &self,
        meta: PluginMeta,
        cwasm: &[u8],
    ) -> Result<LoadedPlugin, PluginError> {
        let module = unsafe { Module::deserialize(&self.engine, cwasm) }
            .map_err(|e| PluginError::Load(format!("deserialize `{}`: {e}", meta.name)))?;
        Ok(self.bind(meta, module))
    }

    /// Load a pre-compiled `.cwasm` blob after verifying its SHA-256 digest.
    ///
    /// Fail-closed: if `sha256_hex` does not match the digest of `cwasm` the
    /// function returns [`PluginError::Load`] and `deserialize` is never called.
    /// This is the safe entry-point for production load paths where the expected
    /// hash is known (e.g. from a pinned manifest or the compiler output).
    ///
    /// `sha256_hex` must be a lowercase hex string of the SHA-256 digest
    /// (64 characters).
    pub fn load_verified(
        &self,
        meta: PluginMeta,
        cwasm: &[u8],
        sha256_hex: &str,
    ) -> Result<LoadedPlugin, PluginError> {
        use sha2::{Digest, Sha256};
        let actual = hex::encode(Sha256::digest(cwasm));
        if actual != sha256_hex {
            return Err(PluginError::Load(format!(
                "cwasm hash mismatch for `{}` (expected {sha256_hex}, got {actual}) — refusing",
                meta.name
            )));
        }
        self.load_precompiled(meta, cwasm)
    }

    /// Wrap an already-built `Module` (used by the compiler path and tests).
    pub fn bind(&self, meta: PluginMeta, module: Module) -> LoadedPlugin {
        LoadedPlugin {
            name: meta.name,
            version: meta.version,
            description: meta.description,
            streaming: meta.streaming,
            signature: meta.signature,
            module,
        }
    }

    /// Run a module against a JSON input string, returning its JSON output.
    /// Sandboxed (empty linker), fuel- and memory-limited. Never panics; a
    /// trap (fuel/memory/guest error) becomes a typed [`PluginError`].
    pub fn run_json(
        &self,
        plugin: &LoadedPlugin,
        caps: &CapabilitySet,
        input: &str,
    ) -> Result<String, PluginError> {
        let mut store = Store::new(&self.engine, StoreState::new(self.max_memory));
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(self.fuel)
            .map_err(|e| self.runtime_err(plugin, format!("set fuel: {e}")))?;

        let linker: Linker<StoreState> = crate::gate::linker(caps, &self.engine);
        let instance = linker
            .instantiate(&mut store, &plugin.module)
            .map_err(|e| PluginError::CapabilityDenied {
                plugin: plugin.name.clone(),
                message: format!("instantiation failed (ungranted import?): {e}"),
            })?;

        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| self.load_err(plugin, "module exports no `memory`"))?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .map_err(|e| self.load_err(plugin, &format!("missing `alloc`: {e}")))?;
        let run = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, "run")
            .map_err(|e| self.load_err(plugin, &format!("missing `run`: {e}")))?;

        self.call_guest_json(plugin, &mut store, &memory, &alloc, &run, input)
    }

    /// Run a **WASI preview1** QuickJS-WASM guest (e.g. a Javy-built module,
    /// stdin, run `_start`, capture its stdout JSON **and** its stderr (logs).
    /// Sandboxed by construction — the WASI context exposes **only**
    /// stdin/stdout/stderr: no preopened directories, no environment, no
    /// network. Fuel- and memory-limited. Stderr is the guest's log
    /// channel (`console.log`, diagnostics); the caller routes it to the
    /// journal so it never enters the pipe.
    pub fn run_wasi_json(
        &self,
        plugin: &LoadedPlugin,
        _caps: &CapabilitySet,
        input: &str,
    ) -> Result<WasiRun, PluginError> {
        let (mut store, stdout, stderr) = self.build_wasi_store(plugin, input)?;
        let (instance, start) = self.instantiate_wasi(plugin, &mut store)?;
        let _ = instance; // not needed after getting `start`

        if let Err(e) = start.call(&mut store, ()) {
            // A WASI command exits via `proc_exit`, surfaced as `I32Exit`;
            // code 0 is a clean exit, anything else (or a real trap) is an error.
            match e.downcast_ref::<wasmtime_wasi::I32Exit>() {
                Some(exit) if exit.0 == 0 => {}
                Some(exit) => {
                    return Err(self.runtime_err(plugin, format!("exited with code {}", exit.0)));
                }
                None => return Err(self.classify_trap(plugin, e)),
            }
        }

        drop(store);
        let output = String::from_utf8(stdout.contents().to_vec())
            .map_err(|e| PluginError::Bridge(format!("guest output: {e}")))?;
        // Logs are diagnostic and untrusted: never fail the run on non-UTF-8
        // stderr — degrade lossily so a malformed log byte can't sink a plugin.
        let logs = String::from_utf8_lossy(&stderr.contents()).into_owned();
        Ok(WasiRun { output, logs })
    }
}

/// Private helpers — error mapping and sub-operation builders.
impl PluginRuntime {
    /// Write `input` bytes into guest memory via the `alloc` export, call
    /// `run`, then read and return the output. Extracted from `run_json` to
    /// keep that function ≤50 lines.
    fn call_guest_json(
        &self,
        plugin: &LoadedPlugin,
        store: &mut Store<StoreState>,
        memory: &wasmtime::Memory,
        alloc: &wasmtime::TypedFunc<i32, i32>,
        run: &wasmtime::TypedFunc<(i32, i32), i64>,
        input: &str,
    ) -> Result<String, PluginError> {
        let bytes = input.as_bytes();
        let len = i32::try_from(bytes.len())
            .map_err(|_| self.runtime_err(plugin, "input too large".to_string()))?;
        let ptr = alloc
            .call(&mut *store, len)
            .map_err(|e| self.classify_trap(plugin, e))?;
        memory
            .write(&mut *store, ptr as usize, bytes)
            .map_err(|e| self.runtime_err(plugin, format!("write input: {e}")))?;

        let packed = run
            .call(&mut *store, (ptr, len))
            .map_err(|e| self.classify_trap(plugin, e))?;
        let out_ptr = ((packed >> 32) & 0xFFFF_FFFF) as usize;
        let out_len = (packed & 0xFFFF_FFFF) as usize;

        // Fail-closed: the guest controls out_len (up to ~4 GiB). A value
        // exceeding the linear-memory ceiling is impossible to back with real
        // guest memory, so reject it before allocating on the host heap.
        if out_len > self.max_memory {
            return Err(self.runtime_err(
                plugin,
                format!(
                    "output length {out_len} exceeds memory limit {}",
                    self.max_memory
                ),
            ));
        }
        let mut out = vec![0u8; out_len];
        memory
            .read(&mut *store, out_ptr, &mut out)
            .map_err(|e| self.runtime_err(plugin, format!("read output: {e}")))?;
        String::from_utf8(out).map_err(|e| PluginError::Bridge(format!("guest output: {e}")))
    }

    /// Set up the WASI store with fuel + memory limits and stdio pipes. Returns
    /// the store plus cloned stdout/stderr handles for post-run extraction.
    /// Extracted from `run_wasi_json` to keep that function ≤50 lines.
    fn build_wasi_store(
        &self,
        plugin: &LoadedPlugin,
        input: &str,
    ) -> Result<
        (
            Store<WasiState>,
            wasmtime_wasi::p2::pipe::MemoryOutputPipe,
            wasmtime_wasi::p2::pipe::MemoryOutputPipe,
        ),
        PluginError,
    > {
        use wasmtime_wasi::WasiCtxBuilder;
        use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};

        let stdin = MemoryInputPipe::new(input.as_bytes().to_vec());
        let stdout = MemoryOutputPipe::new(self.max_memory);
        let stderr = MemoryOutputPipe::new(self.max_memory);
        let wasi = WasiCtxBuilder::new()
            .stdin(stdin)
            .stdout(stdout.clone())
            .stderr(stderr.clone())
            .build_p1();

        let mut store = Store::new(
            &self.engine,
            WasiState {
                limits: StoreLimitsBuilder::new()
                    .memory_size(self.max_memory)
                    .build(),
                wasi,
            },
        );
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(self.wasi_fuel)
            .map_err(|e| self.runtime_err(plugin, format!("set fuel: {e}")))?;
        Ok((store, stdout, stderr))
    }

    /// Add WASI p1 imports to a new linker and instantiate the module. Returns
    /// the instance and the `_start` typed function. Extracted from
    /// `run_wasi_json` to keep that function ≤50 lines.
    fn instantiate_wasi(
        &self,
        plugin: &LoadedPlugin,
        store: &mut Store<WasiState>,
    ) -> Result<(wasmtime::Instance, wasmtime::TypedFunc<(), ()>), PluginError> {
        use wasmtime_wasi::p1;

        let mut linker: Linker<WasiState> = Linker::new(&self.engine);
        p1::add_to_linker_sync(&mut linker, |state: &mut WasiState| &mut state.wasi)
            .map_err(|e| self.load_err(plugin, &format!("wasi linker: {e}")))?;
        let instance = linker
            .instantiate(&mut *store, &plugin.module)
            .map_err(|e| PluginError::CapabilityDenied {
                plugin: plugin.name.clone(),
                message: format!("instantiation failed (ungranted import?): {e}"),
            })?;
        let start = instance
            .get_typed_func::<(), ()>(&mut *store, "_start")
            .map_err(|e| self.load_err(plugin, &format!("missing `_start`: {e}")))?;
        Ok((instance, start))
    }

    /// Map a guest trap to a typed error — fuel/memory exhaustion is a
    /// resource limit (the host is unharmed), anything else is a runtime error.
    fn classify_trap(&self, plugin: &LoadedPlugin, err: wasmtime::Error) -> PluginError {
        if let Some(trap) = err.downcast_ref::<wasmtime::Trap>()
            && matches!(
                trap,
                wasmtime::Trap::OutOfFuel | wasmtime::Trap::MemoryOutOfBounds
            )
        {
            return PluginError::ResourceLimit {
                plugin: plugin.name.clone(),
                message: format!("{trap}"),
            };
        }
        // wasmtime surfaces the store's memory-limit refusal as a plain error.
        let text = err.to_string();
        if text.contains("fuel") || text.contains("memory") {
            return PluginError::ResourceLimit {
                plugin: plugin.name.clone(),
                message: text,
            };
        }
        self.runtime_err(plugin, text)
    }

    fn runtime_err(&self, plugin: &LoadedPlugin, message: String) -> PluginError {
        PluginError::Runtime {
            plugin: plugin.name.clone(),
            message,
        }
    }

    fn load_err(&self, plugin: &LoadedPlugin, message: &str) -> PluginError {
        PluginError::Load(format!("`{}`: {message}", plugin.name))
    }
}

/// Metadata bound to a module at load time (from the manifest).
pub struct PluginMeta {
    pub name: String,
    pub version: String,
    pub description: String,
    pub streaming: bool,
    pub signature: Signature,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::PluginManifest;

    fn meta(name: &str) -> PluginMeta {
        let manifest = PluginManifest::sandbox_default(name);
        PluginMeta {
            name: name.to_string(),
            version: "0.0.0".to_string(),
            description: String::new(),
            streaming: false,
            signature: manifest.to_signature().unwrap(),
        }
    }

    /// A WAT guest implementing the ABI: `run` echoes its input bytes back
    /// (identity transform). Exercises the full host↔guest data path.
    const ECHO_WAT: &str = r#"
        (module
          (memory (export "memory") 2)
          (global $bump (mut i32) (i32.const 1024))
          (func (export "alloc") (param $len i32) (result i32)
            (local $p i32)
            (local.set $p (global.get $bump))
            (global.set $bump (i32.add (global.get $bump) (local.get $len)))
            (local.get $p))
          ;; run(ptr,len): output = input verbatim; return (ptr<<32)|len
          (func (export "run") (param $ptr i32) (param $len i32) (result i64)
            (i64.or
              (i64.shl (i64.extend_i32_u (local.get $ptr)) (i64.const 32))
              (i64.extend_i32_u (local.get $len)))))
    "#;

    /// A WAT guest that imports an effect — must fail to instantiate.
    const FETCH_WAT: &str = r#"
        (module
          (import "env" "fetch" (func $fetch (param i32) (result i32)))
          (memory (export "memory") 1)
          (func (export "alloc") (param i32) (result i32) (i32.const 0))
          (func (export "run") (param i32) (param i32) (result i64) (i64.const 0)))
    "#;

    /// A WAT guest that loops forever — must be stopped by fuel.
    const SPIN_WAT: &str = r#"
        (module
          (memory (export "memory") 1)
          (func (export "alloc") (param i32) (result i32) (i32.const 0))
          (func (export "run") (param i32) (param i32) (result i64)
            (loop $l (br $l))
            (i64.const 0)))
    "#;

    fn module(rt: &PluginRuntime, wat: &str) -> Module {
        Module::new(rt.engine(), wat).unwrap()
    }

    #[test]
    fn echo_guest_round_trips_json_through_memory() {
        let rt = PluginRuntime::new().unwrap();
        let plugin = rt.bind(meta("echo"), module(&rt, ECHO_WAT));
        let input = r#"{"$filesize":2048}"#;
        let out = rt
            .run_json(&plugin, &CapabilitySet::sandbox(), input)
            .unwrap();
        assert_eq!(
            out, input,
            "echo guest returns input verbatim through the ABI"
        );
    }

    #[test]
    fn ungranted_import_fails_closed() {
        let rt = PluginRuntime::new().unwrap();
        let plugin = rt.bind(meta("fetcher"), module(&rt, FETCH_WAT));
        let err = rt
            .run_json(&plugin, &CapabilitySet::sandbox(), "null")
            .unwrap_err();
        assert!(
            matches!(err, PluginError::CapabilityDenied { .. }),
            "a guest importing `fetch` must be refused by the empty linker, got {err:?}"
        );
    }

    #[test]
    fn infinite_loop_stopped_by_fuel_without_host_crash() {
        let rt = PluginRuntime::new().unwrap();
        let plugin = rt.bind(meta("spinner"), module(&rt, SPIN_WAT));
        let err = rt
            .run_json(&plugin, &CapabilitySet::sandbox(), "null")
            .unwrap_err();
        assert!(
            matches!(err, PluginError::ResourceLimit { .. }),
            "an infinite loop must hit the fuel limit, got {err:?}"
        );
        // The host is still alive: a fresh run works.
        let echo = rt.bind(meta("echo"), module(&rt, ECHO_WAT));
        assert!(rt.run_json(&echo, &CapabilitySet::sandbox(), "1").is_ok());
    }

    /// A WASI guest that writes its JSON output to stdout (fd 1) and a log line
    /// to stderr (fd 2) — the shape of a real plugin that `console.log`s. Proves
    /// `run_wasi_json` separates the two: stdout → `output` (the pipe), stderr →
    /// `logs` (the journal).
    const STDIO_WAT: &str = r#"
        (module
          (import "wasi_snapshot_preview1" "fd_write"
            (func $fd_write (param i32 i32 i32 i32) (result i32)))
          (memory (export "memory") 1)
          (data (i32.const 100) "[]")
          (data (i32.const 200) "hello from console\n")
          (func (export "_start")
            ;; iovec at 0 → stdout "[]" (2 bytes)
            (i32.store (i32.const 0) (i32.const 100))
            (i32.store (i32.const 4) (i32.const 2))
            (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16)))
            ;; iovec at 8 → stderr "hello from console\n" (19 bytes)
            (i32.store (i32.const 8) (i32.const 200))
            (i32.store (i32.const 12) (i32.const 19))
            (drop (call $fd_write (i32.const 2) (i32.const 8) (i32.const 1) (i32.const 16)))))
    "#;

    #[test]
    fn wasi_run_separates_stdout_output_from_stderr_logs() {
        let rt = PluginRuntime::new().unwrap();
        let plugin = rt.bind(meta("logger"), module(&rt, STDIO_WAT));
        let run = rt
            .run_wasi_json(&plugin, &CapabilitySet::sandbox(), "{}")
            .unwrap();
        assert_eq!(run.output, "[]", "stdout is the pipe content (Value JSON)");
        assert!(
            run.logs.contains("hello from console"),
            "stderr is captured as logs (→ journal), got {:?}",
            run.logs
        );
        // The log text never leaks into the pipe output.
        assert!(
            !run.output.contains("hello"),
            "logs must not enter the pipe"
        );
    }

    #[test]
    fn precompiled_cwasm_deserializes_and_runs() {
        // Mirrors the production load path: compile (AOT, Cranelift) →
        // serialize to a `.cwasm` blob → deserialize in the runtime → run.
        let rt = PluginRuntime::new().unwrap();
        let compiled = Module::new(rt.engine(), ECHO_WAT).unwrap();
        let cwasm = compiled.serialize().unwrap();
        let plugin = rt.load_precompiled(meta("echo"), &cwasm).unwrap();
        let out = rt
            .run_json(&plugin, &CapabilitySet::sandbox(), r#"{"a":1}"#)
            .unwrap();
        assert_eq!(out, r#"{"a":1}"#);
    }
}

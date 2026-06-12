// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! WASI plugin acceptance (where_geo-equivalent). The fixture `where_big.wasm` is the
//! Javy-compiled `where_big.js` (vendored under tests/fixtures/) — a filter that
//! keeps table rows whose `size` ($filesize) ≥ a `min_size` argument.
//!
//! This proves the locked engine decision (QuickJS-in-WASM via wasmtime)
//! actually executes JS, the Value↔JS bridge preserves rich types (Filesize)
//! both ways, and arguments reach the plugin — the acceptance criterion
//! "where_geo round-trip ... preserves rich types".

use std::sync::Arc;

use indexmap::IndexMap;
use orkia_plugin::runtime::{PluginMeta, PluginRuntime};
use orkia_plugin::{PluginCommand, PluginManifest};
use orkia_shell_types::CapabilitySet;
use orkia_shell_types::Value;
use orkia_shell_types::exec::command::{Command, CommandCtx, EvaluatedCall};
use orkia_shell_types::exec::pipeline_data::PipelineData;
use wasmtime::Module;

/// The Javy-compiled QuickJS-WASM pilot plugin (real engine module).
const WHERE_BIG: &[u8] = include_bytes!("fixtures/where_big.wasm");

const MANIFEST: &str = r#"
    [plugin]
    name = "where_big"
    version = "0.1.0"
    [command]
    input_type = "list<record>"
    output_type = "list<record>"
    [command.args]
    min_size = { type = "filesize" }
"#;

fn load(rt: &PluginRuntime) -> orkia_plugin::LoadedPlugin {
    // Compile the Javy `.wasm` (Cranelift, in the test build) — the production
    // path precompiles to `.cwasm` at `plugin add` time; same artifact.
    let module = Module::new(rt.engine(), WHERE_BIG).expect("compile fixture");
    let manifest = PluginManifest::parse(MANIFEST).expect("manifest");
    rt.bind(
        PluginMeta {
            name: "where_big".to_string(),
            version: "0.1.0".to_string(),
            description: "filter rows by size".to_string(),
            streaming: false,
            signature: manifest.to_signature().expect("signature"),
        },
        module,
    )
}

fn record(name: &str, size: i64) -> Value {
    let mut m = IndexMap::new();
    m.insert("name".to_string(), Value::String(name.to_string()));
    m.insert("size".to_string(), Value::Filesize(size));
    Value::Record(m)
}

#[test]
fn quickjs_engine_filters_via_stdin_stdout() {
    let rt = PluginRuntime::new().expect("runtime");
    let plugin = load(&rt);
    let envelope = r#"{"input":[
        {"name":"a","size":{"$filesize":100}},
        {"name":"b","size":{"$filesize":5000}}
    ],"call":{"positional":[],"named":{"min_size":{"$filesize":1000}}}}"#;

    let out = rt
        .run_wasi_json(&plugin, &CapabilitySet::sandbox(), envelope)
        .expect("quickjs run")
        .output;
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
    let arr = parsed.as_array().expect("array");
    assert_eq!(arr.len(), 1, "only the 5000-byte row passes >= 1000");
    assert_eq!(arr[0]["name"], "b");
    assert_eq!(arr[0]["size"]["$filesize"], 5000);
}

#[tokio::test]
async fn plugin_command_end_to_end_preserves_rich_types() {
    let rt = Arc::new(PluginRuntime::new().expect("runtime"));
    let plugin = Arc::new(load(rt.as_ref()));
    let cmd = PluginCommand::new(plugin, rt, CapabilitySet::sandbox());

    let rows = Value::List(vec![record("a", 100), record("b", 5000), record("c", 2000)]);
    let mut named = IndexMap::new();
    named.insert("min_size".to_string(), Some(Value::Filesize(1000)));
    let call = EvaluatedCall {
        head: "where_big".to_string(),
        positional: Vec::new(),
        named,
    };
    let ctx = CommandCtx {
        cwd: std::path::PathBuf::from("."),
        env: std::collections::HashMap::new(),
        data_dir: std::path::PathBuf::from("."),
        agents: Vec::new(),
        jobs: Vec::new(),
        journal: None,
        auth: None,
        attention: Vec::new(),
        attention_control: None,
        capabilities: CapabilitySet::sandbox(),
    };

    let out = cmd
        .run(&ctx, &call, PipelineData::Value(rows))
        .await
        .expect("plugin command run");
    let value = out.into_value().await.expect("collect");
    let list = match value {
        Value::List(l) => l,
        other => panic!("expected list, got {other:?}"),
    };
    assert_eq!(list.len(), 2, "b (5000) and c (2000) pass >= 1000");
    let names: Vec<&str> = list
        .iter()
        .filter_map(|r| match r.as_record().and_then(|m| m.get("name")) {
            Some(Value::String(s)) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["b", "c"]);
    // The rich Filesize type survived the full Value→JS→Value round trip.
    let first = list[0].as_record().expect("record");
    assert_eq!(first.get("size"), Some(&Value::Filesize(5000)));
}

const EFFECT_PROBE: &[u8] = include_bytes!("fixtures/effect_probe.wasm");
const SPIN: &[u8] = include_bytes!("fixtures/spin.wasm");
/// A Javy plugin that ACTIVELY ATTEMPTS effects (fetch / require / env / XHR) at
/// runtime and reports, per attempt, `"SUCCEEDED"` or `"blocked:<reason>"`.
const EFFECT_ATTEMPT: &[u8] = include_bytes!("fixtures/effect_attempt.wasm");

fn load_bytes(rt: &PluginRuntime, name: &str, wasm: &[u8]) -> orkia_plugin::LoadedPlugin {
    let module = Module::new(rt.engine(), wasm).expect("compile fixture");
    let signature = PluginManifest::sandbox_default(name)
        .to_signature()
        .expect("sig");
    rt.bind(
        PluginMeta {
            name: name.to_string(),
            version: "0.0.0".to_string(),
            description: String::new(),
            streaming: false,
            signature,
        },
        module,
    )
}

/// On the PRODUCTION path (`run_wasi_json`): a real Javy/QuickJS
/// plugin has **no effect surface** — fetch / require / process / XMLHttpRequest /
/// WebSocket are all `undefined`. Nothing can reach the network, FS, or host: the
/// sandbox is proven empty on the path users actually take, not by construction.
#[test]
fn wasi_quickjs_plugin_has_no_effect_surface() {
    let rt = PluginRuntime::new().expect("runtime");
    let plugin = load_bytes(&rt, "probe", EFFECT_PROBE);
    let out = rt
        .run_wasi_json(&plugin, &CapabilitySet::sandbox(), "{}")
        .expect("probe runs")
        .output;
    let probe: serde_json::Value = serde_json::from_str(&out).expect("json");
    for api in [
        "fetch",
        "require",
        "process",
        "XMLHttpRequest",
        "global_fetch",
        "WebSocket",
    ] {
        assert_eq!(
            probe[api], "undefined",
            "effect API `{api}` must be absent from the sandbox, got {:?}",
            probe[api]
        );
    }
}

/// a Javy plugin that **actively attempts** effects on the production
/// `run_wasi_json` path — `fetch`, `require("fs")`, `process.env`,
/// `new XMLHttpRequest()` — has **every** attempt fail-closed. Stronger than
/// `wasi_quickjs_plugin_has_no_effect_surface` (which proves the APIs are
/// absent): this proves a plugin that *tries* to escape is blocked, on the path
/// users actually take, and that nothing leaks out of the sandbox.
///
/// Plain `#[test]` (not `#[tokio::test]`): `run_wasi_json` is sync and the
/// wasmtime-wasi sync path panics if driven from a tokio worker — the same
/// reason `PluginCommand::run` offloads it to a dedicated OS thread.
#[test]
fn wasi_javy_plugin_effect_attempt_fails_closed() {
    let rt = PluginRuntime::new().expect("runtime");
    let plugin = load_bytes(&rt, "effect_attempt", EFFECT_ATTEMPT);
    let out = rt
        .run_wasi_json(&plugin, &CapabilitySet::sandbox(), "{}")
        .expect("attempt plugin runs (the host is unharmed)")
        .output;
    let report: serde_json::Value = serde_json::from_str(&out).expect("json");

    for effect in ["fetch", "require", "env", "xhr"] {
        let outcome = report[effect].as_str().unwrap_or("<missing>");
        assert!(
            outcome.starts_with("blocked:"),
            "effect `{effect}` must be blocked-closed on the production path, got `{outcome}`"
        );
        assert_ne!(
            outcome, "SUCCEEDED",
            "effect `{effect}` escaped the sandbox — fail-closed violated"
        );
    }
}

/// On the PRODUCTION path: a runaway Javy plugin (`while(true)`)
/// is stopped by the fuel limit as a typed `ResourceLimit` — the host is not
/// harmed, and a fresh plugin still runs afterward.
#[test]
fn wasi_quickjs_runaway_stopped_by_fuel() {
    // Small WASI fuel budget so the infinite loop trips quickly (still enough
    // for QuickJS engine init — verified by the sanity run below).
    let rt = PluginRuntime::new()
        .expect("runtime")
        .with_wasi_fuel(2_000_000_000);
    let spinner = load_bytes(&rt, "spin", SPIN);
    let err = rt
        .run_wasi_json(&spinner, &CapabilitySet::sandbox(), "{}")
        .expect_err("runaway must be stopped");
    assert!(
        matches!(err, orkia_plugin::PluginError::ResourceLimit { .. }),
        "runaway plugin must hit the resource limit, got {err:?}"
    );
    // Host survives: a fresh plugin on the same runtime still works.
    let probe = load_bytes(&rt, "probe", EFFECT_PROBE);
    assert!(
        rt.run_wasi_json(&probe, &CapabilitySet::sandbox(), "{}")
            .is_ok(),
        "host unharmed: a fresh plugin runs after the runaway was stopped \
         (also confirms 2e9 fuel suffices for QuickJS init)"
    );
}

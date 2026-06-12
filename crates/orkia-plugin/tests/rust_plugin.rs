// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! The fixture `where_big_rs.wasm` is the `crates/orkia-plugin-rust-fixture`
//! crate (a `#[orkia::command]` filter) compiled to `wasm32-wasip1` and
//! vendored here. It is the Rust counterpart of the TS `where_big.wasm` used by
//! `wasi_plugin.rs` — same behaviour, different source language.
//!
//! This proves the polyglot thesis: a Rust plugin (a) installs/runs via
//! the SAME `run_wasi_json` path as a TS plugin, (b) produces `Value`s
//! indiscernible from the TS version (the rich `Filesize` round-trips through
//! the shared tagged-JSON bridge), and (c) is held to the SAME total sandbox by
//! default. To regenerate: `cargo build --target wasm32-wasip1 --release` in
//! `crates/orkia-plugin-rust-fixture`, then copy the `.wasm` here.

use std::sync::Arc;

use indexmap::IndexMap;
use orkia_plugin::runtime::{PluginMeta, PluginRuntime};
use orkia_plugin::{CapabilitySet, PluginCommand, PluginManifest};
use orkia_shell_types::Value;
use orkia_shell_types::exec::command::{Command, CommandCtx, EvaluatedCall};
use orkia_shell_types::exec::pipeline_data::PipelineData;
use wasmtime::Module;

/// The Rust plugin, compiled to `wasm32-wasip1` (a preview1 command module:
/// reads stdin, writes stdout — the same shape Javy produces for TS).
const WHERE_BIG_RS: &[u8] = include_bytes!("fixtures/where_big_rs.wasm");

const MANIFEST: &str = r#"
    [plugin]
    name = "where_big_rs"
    version = "0.1.0"
    [command]
    input_type = "list<record>"
    output_type = "list<record>"
    [command.args]
    min_size = { type = "filesize" }
"#;

fn load(rt: &PluginRuntime) -> orkia_plugin::LoadedPlugin {
    let module = Module::new(rt.engine(), WHERE_BIG_RS).expect("compile rust fixture");
    let manifest = PluginManifest::parse(MANIFEST).expect("manifest");
    rt.bind(
        PluginMeta {
            name: "where_big_rs".to_string(),
            version: "0.1.0".to_string(),
            description: "filter rows by size (Rust)".to_string(),
            streaming: false,
            signature: manifest.to_signature().expect("signature"),
        },
        module,
    )
}

/// Same fixture, bound as a **streaming** plugin (`streaming = true`) — the
/// guest is byte-for-byte identical; only the host frontier changes.
/// Proves streaming needs no guest-side change.
fn load_streaming(rt: &PluginRuntime) -> orkia_plugin::LoadedPlugin {
    let module = Module::new(rt.engine(), WHERE_BIG_RS).expect("compile rust fixture");
    let manifest = PluginManifest::parse(MANIFEST).expect("manifest");
    rt.bind(
        PluginMeta {
            name: "where_big_rs".to_string(),
            version: "0.1.0".to_string(),
            description: "filter rows by size (Rust, streaming)".to_string(),
            streaming: true,
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

fn sandbox_ctx() -> CommandCtx {
    CommandCtx {
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
    }
}

/// A Rust plugin runs through the SAME `run_wasi_json`
/// path. Direct envelope form (mirrors `wasi_plugin.rs::quickjs_engine_…`).
#[test]
fn rust_plugin_filters_via_stdin_stdout() {
    let rt = PluginRuntime::new().expect("runtime");
    let plugin = load(&rt);
    let envelope = r#"{"input":[
        {"name":"a","size":{"$filesize":100}},
        {"name":"b","size":{"$filesize":5000}}
    ],"call":{"positional":[],"named":{"min_size":{"$filesize":1000}}}}"#;

    let out = rt
        .run_wasi_json(&plugin, &CapabilitySet::sandbox(), envelope)
        .expect("rust plugin run")
        .output;
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
    let arr = parsed.as_array().expect("array");
    assert_eq!(arr.len(), 1, "only the 5000-byte row passes >= 1000");
    assert_eq!(arr[0]["name"], "b");
    // The rich Filesize tag is preserved — indiscernible from TS.
    assert_eq!(arr[0]["size"]["$filesize"], 5000);
}

/// Through the full `PluginCommand` → bridge → host path:
/// the Rust plugin's output is indiscernible from the TS `where_big` — same
/// rows kept, same rich `Filesize` type on the host side.
#[tokio::test]
async fn rust_plugin_command_indiscernible_from_ts() {
    let rt = Arc::new(PluginRuntime::new().expect("runtime"));
    let plugin = Arc::new(load(rt.as_ref()));
    let cmd = PluginCommand::new(plugin, rt, CapabilitySet::sandbox());

    let rows = Value::List(vec![record("a", 100), record("b", 5000), record("c", 2000)]);
    let mut named = IndexMap::new();
    named.insert("min_size".to_string(), Some(Value::Filesize(1000)));
    let call = EvaluatedCall {
        head: "where_big_rs".to_string(),
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
        .expect("rust plugin command run");
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
    // The rich Filesize type survived the full Value→JSON→(Rust plugin)→JSON→Value
    // round trip — identical to the TS plugin's result.
    let first = list[0].as_record().expect("record");
    assert_eq!(first.get("size"), Some(&Value::Filesize(5000)));
}

/// A Rust plugin is held to the SAME governance — the manifest
/// with no `[capabilities]` yields a total-sandbox grant, exactly like a TS one.
#[test]
fn rust_plugin_default_grant_is_total_sandbox() {
    let manifest = PluginManifest::parse(MANIFEST).expect("manifest");
    assert!(
        manifest.granted_capabilities().is_total_sandbox(),
        "no declared capabilities ⇒ total sandbox (fail-closed), language-agnostic"
    );
}

/// A `streaming = true` plugin does NOT
/// materialize the whole input — a downstream `first 5` stops the plugin after
/// a single chunk. Mirrors the `early-termination` test: we count how
/// many rows the upstream is pulled for and assert it's bounded by one chunk,
/// far below the (large) total available.
#[tokio::test]
async fn streaming_plugin_early_terminates_on_downstream_first() {
    use futures::StreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let rt = Arc::new(PluginRuntime::new().expect("runtime"));
    let plugin = Arc::new(load_streaming(rt.as_ref()));
    let cmd = PluginCommand::new(plugin, rt, CapabilitySet::sandbox());

    // A large upstream (10_000 rows, all ≥ min so all pass the filter) that
    // counts every row actually pulled.
    const TOTAL: usize = 10_000;
    let pulled = Arc::new(AtomicUsize::new(0));
    let pc = pulled.clone();
    let upstream = futures::stream::unfold(0usize, move |i| {
        let pc = pc.clone();
        async move {
            if i >= TOTAL {
                return None;
            }
            pc.fetch_add(1, Ordering::SeqCst);
            Some((Ok(record("row", 5000)), i + 1))
        }
    })
    .boxed();

    let mut named = IndexMap::new();
    named.insert("min_size".to_string(), Some(Value::Filesize(1000)));
    let call = EvaluatedCall {
        head: "where_big_rs".to_string(),
        positional: Vec::new(),
        named,
    };

    let out = cmd
        .run(&sandbox_ctx(), &call, PipelineData::ListStream(upstream))
        .await
        .expect("streaming run");
    let mut stream = match out {
        PipelineData::ListStream(s) => s,
        _ => panic!("expected ListStream"),
    };

    // Take only the first 5 rows, then drop the stream (the `first 5` behavior).
    let mut got = Vec::new();
    for _ in 0..5 {
        got.push(stream.next().await.expect("row").expect("ok"));
    }
    drop(stream);

    assert_eq!(got.len(), 5, "downstream took exactly 5 rows");
    let pulled = pulled.load(Ordering::SeqCst);
    assert!(
        pulled <= 256,
        "early termination: upstream pulled {pulled} rows (≤ one 256-chunk), \
         NOT the full {TOTAL} — the plugin did not materialize everything"
    );
    assert!(pulled < TOTAL, "must not have drained the whole input");
}

/// The streaming frontier produces the SAME rows as batch — streaming is a
/// performance/early-termination property, not a semantic change.
#[tokio::test]
async fn streaming_and_batch_agree_on_output() {
    use futures::StreamExt;

    let rt = Arc::new(PluginRuntime::new().expect("runtime"));
    let rows = vec![record("a", 100), record("b", 5000), record("c", 2000)];
    let mut named = IndexMap::new();
    named.insert("min_size".to_string(), Some(Value::Filesize(1000)));
    let call = EvaluatedCall {
        head: "where_big_rs".to_string(),
        positional: Vec::new(),
        named,
    };

    // Streaming path.
    let scmd = PluginCommand::new(
        Arc::new(load_streaming(rt.as_ref())),
        rt.clone(),
        CapabilitySet::sandbox(),
    );
    let sout = scmd
        .run(
            &sandbox_ctx(),
            &call,
            PipelineData::Value(Value::List(rows.clone())),
        )
        .await
        .expect("streaming run");
    let mut s = match sout {
        PipelineData::ListStream(s) => s,
        _ => panic!("expected ListStream"),
    };
    let mut streamed = Vec::new();
    while let Some(item) = s.next().await {
        streamed.push(item.expect("ok"));
    }

    // Batch path.
    let bcmd = PluginCommand::new(Arc::new(load(rt.as_ref())), rt, CapabilitySet::sandbox());
    let bout = bcmd
        .run(
            &sandbox_ctx(),
            &call,
            PipelineData::Value(Value::List(rows)),
        )
        .await
        .expect("batch run");
    let batched = match bout.into_value().await.expect("collect") {
        Value::List(l) => l,
        other => panic!("expected list, got {other:?}"),
    };

    assert_eq!(
        streamed, batched,
        "streaming and batch keep the same rows (b, c)"
    );
    assert_eq!(streamed.len(), 2);
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Adapting a loaded plugin into an EXEC-CORE `Command`.
//!
//! A `PluginCommand` registers like any other command. The kernel type-checks
//! the pipeline against the plugin's declared `Signature` **before** `run` is
//! ever called, so a `ByteStream` into a `list<record>` plugin is refused
//! upstream as a `TypeMismatch`, never at execution.
//!
//! Two frontiers, chosen by the manifest's `streaming` flag:
//! - **batch** (`streaming = false`, the V1 default): drain the whole
//!   input into one `Value`, one guest call, return its `Value`.
//! - **streaming** (`streaming = true`): pull `STREAM_CHUNK`
//!   items at a time, run the guest per chunk, yield lazily; a downstream
//!   `first N` drops the stream and stops the plugin early. The guest contract
//!   is identical (one-shot per chunk) — no guest-side change.

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use orkia_shell_types::exec::command::{Command, CommandCtx, EvaluatedCall};
use orkia_shell_types::exec::pipeline_data::{PipelineData, ValueStream};
use orkia_shell_types::extensions::JournalEnvelopeHook;
use orkia_shell_types::journal::{EventType, JournalEnvelope};
use orkia_shell_types::{CapabilitySet, ExecError, Signature, Value};
use serde_json::json;

use crate::bridge::{json_to_value, value_to_json};
use crate::error::PluginError;
use crate::runtime::{LoadedPlugin, PluginRuntime, WasiRun};

/// Items pulled per guest invocation in streaming mode (chunking
/// amortizes the per-call frontier cost while preserving streaming). The guest
/// sees a batch of up to this many rows per call.
const STREAM_CHUNK: usize = 256;

/// A loaded plugin exposed as an EXEC-CORE command.
pub struct PluginCommand {
    plugin: Arc<LoadedPlugin>,
    runtime: Arc<PluginRuntime>,
    /// The granted effect capabilities (unified `CapabilitySet`). V1: the
    /// approved set is recorded and gates instantiation; effect imports stay
    /// empty (sandbox) regardless — effects route through MCP.
    caps: CapabilitySet,
}

impl PluginCommand {
    /// Build a command for `plugin` with its granted `CapabilitySet` (from the
    /// user-approved manifest; V1: total sandbox).
    pub fn new(
        plugin: Arc<LoadedPlugin>,
        runtime: Arc<PluginRuntime>,
        caps: CapabilitySet,
    ) -> Self {
        Self {
            plugin,
            runtime,
            caps,
        }
    }
}

#[async_trait]
impl Command for PluginCommand {
    fn signature(&self) -> Signature {
        self.plugin.signature.clone()
    }

    fn description(&self) -> &str {
        &self.plugin.description
    }

    fn is_streaming(&self) -> bool {
        self.plugin.streaming
    }

    async fn run(
        &self,
        ctx: &CommandCtx,
        call: &EvaluatedCall,
        input: PipelineData,
    ) -> Result<PipelineData, ExecError> {
        // `streaming = true` ⇒ chunked lazy frontier (early-termination aware);
        // otherwise the V1 collecting frontier (drain all → one guest call).
        if self.plugin.streaming {
            self.run_streaming(ctx, call, input).await
        } else {
            self.run_batch(ctx, call, input).await
        }
    }
}

impl PluginCommand {
    /// V1 collecting frontier: drain the whole input into one `Value`,
    /// run the guest once, return its output `Value`.
    async fn run_batch(
        &self,
        ctx: &CommandCtx,
        call: &EvaluatedCall,
        input: PipelineData,
    ) -> Result<PipelineData, ExecError> {
        let value = input.into_value().await?;
        let envelope = encode_envelope(&self.plugin.name, &value, &call_to_json(call))?;
        let WasiRun { output, logs } = run_guest(
            self.runtime.clone(),
            self.plugin.clone(),
            self.caps.clone(),
            envelope,
        )
        .await?;
        // stderr (console.log / diagnostics) → journal, tagged by
        // plugin name; only the returned Value array below crosses the pipe.
        route_logs(ctx.journal.as_ref(), &self.plugin.name, &logs);
        Ok(PipelineData::Value(parse_output(
            &self.plugin.name,
            &output,
        )?))
    }

    /// Chunked streaming frontier (opt-in `streaming = true`):
    /// pull up to `STREAM_CHUNK` items, run the guest on that chunk, yield its
    /// results lazily, repeat. Dropping the returned stream (a downstream
    /// `first N`) stops the pull — the plugin processes only the chunks needed,
    /// never the whole input. The guest contract is unchanged: each chunk is the
    /// same one-shot batch call, so no guest-side change is required.
    async fn run_streaming(
        &self,
        ctx: &CommandCtx,
        call: &EvaluatedCall,
        input: PipelineData,
    ) -> Result<PipelineData, ExecError> {
        let state = ChunkState {
            upstream: into_value_stream(input),
            pending: VecDeque::new(),
            upstream_done: false,
            guest: GuestCtx {
                runtime: self.runtime.clone(),
                plugin: self.plugin.clone(),
                caps: self.caps.clone(),
                call_json: call_to_json(call),
                journal: ctx.journal.clone(),
            },
        };
        let out = stream::unfold(state, |mut st| async move {
            loop {
                if let Some(value) = st.pending.pop_front() {
                    return Some((Ok(value), st));
                }
                if st.upstream_done {
                    return None;
                }
                let mut chunk = Vec::with_capacity(STREAM_CHUNK);
                while chunk.len() < STREAM_CHUNK {
                    match st.upstream.next().await {
                        Some(Ok(value)) => chunk.push(value),
                        Some(Err(e)) => return Some((Err(e), st)),
                        None => {
                            st.upstream_done = true;
                            break;
                        }
                    }
                }
                if chunk.is_empty() {
                    return None;
                }
                match run_chunk(&st.guest, chunk).await {
                    Ok(items) => st.pending.extend(items),
                    Err(e) => return Some((Err(e), st)),
                }
            }
        });
        Ok(PipelineData::ListStream(out.boxed()))
    }
}

/// The lazy state threaded through the streaming `unfold`: the pull-stream plus
/// the (Sync) guest context. Kept separate because the boxed stream is `Send`
/// but not `Sync`, so `run_chunk` borrows only [`GuestCtx`] across its await.
struct ChunkState {
    upstream: ValueStream,
    pending: VecDeque<Value>,
    upstream_done: bool,
    guest: GuestCtx,
}

/// The per-invocation context shared across chunks — all `Sync`, so a `&GuestCtx`
/// is safe to hold across the guest await inside the streaming future.
struct GuestCtx {
    runtime: Arc<PluginRuntime>,
    plugin: Arc<LoadedPlugin>,
    caps: CapabilitySet,
    call_json: serde_json::Value,
    journal: Option<Arc<dyn JournalEnvelopeHook>>,
}

/// Run the guest on one chunk and return its output rows. Same envelope shape as
/// batch (`{input:[chunk], call}`); logs are routed to the journal per chunk.
async fn run_chunk(guest: &GuestCtx, chunk: Vec<Value>) -> Result<Vec<Value>, ExecError> {
    let envelope = encode_envelope(&guest.plugin.name, &Value::List(chunk), &guest.call_json)?;
    let WasiRun { output, logs } = run_guest(
        guest.runtime.clone(),
        guest.plugin.clone(),
        guest.caps.clone(),
        envelope,
    )
    .await?;
    route_logs(guest.journal.as_ref(), &guest.plugin.name, &logs);
    match parse_output(&guest.plugin.name, &output)? {
        Value::List(items) => Ok(items),
        other => Ok(vec![other]),
    }
}

/// Normalize pipeline input into a lazy item stream for chunked streaming.
fn into_value_stream(input: PipelineData) -> ValueStream {
    match input {
        PipelineData::ListStream(s) => s,
        PipelineData::Value(Value::List(items)) => stream::iter(items.into_iter().map(Ok)).boxed(),
        PipelineData::Value(value) => stream::iter(std::iter::once(Ok(value))).boxed(),
        PipelineData::Empty => stream::empty().boxed(),
        // A ByteStream into a structured plugin is refused upstream by the type
        // check; treat as empty rather than panicking (defensive).
        PipelineData::ByteStream(_) => stream::empty().boxed(),
    }
}

/// Build the `{input, call}` JSON envelope the guest reads on stdin.
fn encode_envelope(
    name: &str,
    value: &Value,
    call_json: &serde_json::Value,
) -> Result<String, ExecError> {
    let envelope = json!({ "input": value_to_json(value), "call": call_json });
    serde_json::to_string(&envelope).map_err(|e| ExecError::Runtime {
        command: name.to_string(),
        message: format!("serialize input: {e}"),
    })
}

/// Parse a guest's stdout JSON into a `Value`.
fn parse_output(name: &str, output: &str) -> Result<Value, ExecError> {
    let parsed: serde_json::Value =
        serde_json::from_str(output).map_err(|e| ExecError::Runtime {
            command: name.to_string(),
            message: format!("parse output: {e}"),
        })?;
    Ok(json_to_value(&parsed))
}

/// Run a WASI guest off the async executor. `run_wasi_json` is synchronous,
/// CPU-bound, fuel-bounded work with no ambient tokio handle of its own, so it
/// must not run on an async worker. `spawn_blocking` runs it on tokio's
/// **bounded** blocking pool — threads are reused across chunks and the pool is
/// capped — so a high-cardinality stream terminated early by a downstream
/// `first N` cannot accumulate unbounded OS threads the way a raw
/// `std::thread::spawn` per chunk would (REF-005). Shared by the batch and the
/// streaming (per-chunk) paths.
async fn run_guest(
    runtime: Arc<PluginRuntime>,
    plugin: Arc<LoadedPlugin>,
    caps: CapabilitySet,
    input_json: String,
) -> Result<WasiRun, ExecError> {
    let name = plugin.name.clone();
    tokio::task::spawn_blocking(move || runtime.run_wasi_json(&plugin, &caps, &input_json))
        .await
        .map_err(|_| ExecError::Runtime {
            command: name.clone(),
            message: "plugin worker task panicked".to_string(),
        })?
        .map_err(|e| plugin_to_exec(&name, e))
}

/// Route a guest's captured stderr to the journal, one envelope per non-empty
/// line, tagged by plugin name. No-op when there is no journal
/// sink or no log output. Never enters the pipe.
fn route_logs(journal: Option<&Arc<dyn JournalEnvelopeHook>>, plugin: &str, logs: &str) {
    let Some(journal) = journal else {
        return;
    };
    for line in logs.lines().filter(|l| !l.trim().is_empty()) {
        let mut env = JournalEnvelope::now(EventType::Shell);
        env.event = Some("plugin.console".to_string());
        env.source = Some(plugin.to_string());
        env.message = Some(line.to_string());
        journal.on_envelope(&env);
    }
}

/// Encode an `EvaluatedCall` for the guest envelope.
fn call_to_json(call: &EvaluatedCall) -> serde_json::Value {
    let positional: Vec<serde_json::Value> = call.positional.iter().map(value_to_json).collect();
    let mut named = serde_json::Map::new();
    for (key, value) in &call.named {
        let json = match value {
            Some(v) => value_to_json(v),
            None => serde_json::Value::Bool(true),
        };
        named.insert(key.clone(), json);
    }
    json!({ "positional": positional, "named": named })
}

fn plugin_to_exec(name: &str, err: PluginError) -> ExecError {
    ExecError::Runtime {
        command: name.to_string(),
        message: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::PluginManifest;
    use crate::runtime::PluginMeta;
    use orkia_shell_types::Type;

    // A trivial WAT guest is enough to construct a `PluginCommand` for the
    // trait-surface check (no execution here). The full Command→bridge→QuickJS
    // round trip runs against a real Javy plugin in `tests/wasi_plugin.rs`.
    const STUB_WAT: &str = r#"(module (memory (export "memory") 1)
        (func (export "_start")))"#;

    fn stub_command() -> PluginCommand {
        let runtime = Arc::new(PluginRuntime::new().unwrap());
        let module = wasmtime::Module::new(runtime.engine(), STUB_WAT).unwrap();
        let signature = PluginManifest::sandbox_default("stub")
            .to_signature()
            .unwrap();
        let plugin = Arc::new(runtime.bind(
            PluginMeta {
                name: "stub".to_string(),
                version: "0.0.0".to_string(),
                description: "stub".to_string(),
                streaming: false,
                signature,
            },
            module,
        ));
        PluginCommand::new(plugin, runtime, CapabilitySet::sandbox())
    }

    #[test]
    fn signature_reflects_manifest_io_types() {
        let sig = stub_command().signature();
        // sandbox default ⇒ any → any (the engine type-checks against this
        // BEFORE run, refusing a mismatched input upstream).
        assert_eq!(sig.io_types, vec![(Type::Any, Type::Any)]);
    }

    /// A journal sink that records every envelope it receives.
    #[derive(Default)]
    struct RecordingJournal {
        envs: std::sync::Mutex<Vec<JournalEnvelope>>,
    }
    impl orkia_shell_types::extensions::JournalEnvelopeHook for RecordingJournal {
        fn on_envelope(&self, env: &JournalEnvelope) {
            self.envs.lock().unwrap().push(env.clone());
        }
    }

    fn ctx_with(
        journal: Arc<dyn orkia_shell_types::extensions::JournalEnvelopeHook>,
    ) -> CommandCtx {
        CommandCtx {
            cwd: std::path::PathBuf::from("."),
            env: std::collections::HashMap::new(),
            data_dir: std::path::PathBuf::from("."),
            agents: Vec::new(),
            jobs: Vec::new(),
            journal: Some(journal),
            auth: None,
            attention: Vec::new(),
            attention_control: None,
            capabilities: CapabilitySet::sandbox(),
        }
    }

    #[test]
    fn console_logs_route_to_journal_tagged_by_plugin() {
        let journal = Arc::new(RecordingJournal::default());
        let ctx = ctx_with(journal.clone());
        route_logs(ctx.journal.as_ref(), "where_geo", "line one\n\nline two\n");

        let envs = journal.envs.lock().unwrap();
        // Blank lines are skipped; each real line is one envelope.
        assert_eq!(envs.len(), 2, "one envelope per non-empty log line");
        assert_eq!(envs[0].source.as_deref(), Some("where_geo"));
        assert_eq!(envs[0].event.as_deref(), Some("plugin.console"));
        assert_eq!(envs[0].message.as_deref(), Some("line one"));
        assert_eq!(envs[1].message.as_deref(), Some("line two"));
    }

    #[test]
    fn route_logs_without_journal_is_noop() {
        // No journal sink ⇒ no panic, nothing emitted (the common case).
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
        route_logs(ctx.journal.as_ref(), "p", "ignored\n");
    }
}

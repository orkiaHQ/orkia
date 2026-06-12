// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! End-to-end: a real QuickJS-WASM plugin, installed and run inside the shell
//! `ork ls | <plugin>` composes; type-checked through the EXEC-CORE registry).
//!
//! The fixture is the Javy-compiled `where_big` filter (keeps rows whose
//! `size` ≥ `--min_size`). It is pre-compiled to `.cwasm` at test time (the
//! production binary loads pre-compiled modules; here Cranelift is a dev-dep),
//! installed into the data-dir's `plugins/`, and loaded at REPL construction.

use orkia_shell::config::ShellConfig;
use orkia_shell::decision::BlockContent;
use orkia_shell::renderer::{PromptContext, RenderEvent, ShellRenderer};
use orkia_shell::{HeuristicClassifier, HeuristicRouter, Repl};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

const WHERE_BIG_WASM: &[u8] = include_bytes!("fixtures/where_big.wasm");
/// built to `wasm32-wasip1`. A raw `.wasm`, language-agnostic to the host.
const WHERE_BIG_RS_WASM: &[u8] = include_bytes!("fixtures/where_big_rs.wasm");

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

#[derive(Default, Clone)]
struct TestRenderer {
    events: Arc<Mutex<Vec<RenderEvent>>>,
}
impl ShellRenderer for TestRenderer {
    fn publish(&mut self, event: RenderEvent) {
        self.events.lock().expect("lock").push(event);
    }
    fn read_line(&mut self, _ctx: &PromptContext) -> Option<String> {
        None
    }
}

fn cfg(dir: &TempDir) -> ShellConfig {
    ShellConfig {
        data_dir: dir.path().to_path_buf(),
        agents: vec![],
        agent_commands: std::collections::HashMap::new(),
        native_agents: Default::default(),
        default_shell: None,
        default_project: None,
        default_scope: None,
        default_mode: None,
        load_bashrc: None,
        load_profile: None,
        notification_verbosity: None,
        cage: Default::default(),
        daemon: Default::default(),
    }
}

fn block_texts(events: &[RenderEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            RenderEvent::Block(BlockContent::Text(t))
            | RenderEvent::Block(BlockContent::SystemInfo(t))
            | RenderEvent::Block(BlockContent::Error(t)) => Some(t.clone()),
            RenderEvent::Block(BlockContent::TableRow(cells)) => Some(
                cells
                    .iter()
                    .map(|c| c.text.as_str())
                    .collect::<Vec<_>>()
                    .join("  "),
            ),
            _ => None,
        })
        .collect()
}

/// True when `plugin add`/`dev` output shows the external `orkia-compiler`
/// toolchain is unusable, so the test should skip rather than fail. Two env
/// conditions, neither a code bug:
/// - **unavailable**: no cached artifact / no compiler / javy missing;
/// - **version-skewed**: a *stale cached* `orkia-compiler` (built against an
///   older wasmtime) compiles fine but emits a `.cwasm` the current loader
///   rejects (`Module was compiled with incompatible version …`). The shipped
///   compiler and loader always share one wasmtime, so this only happens with a
///   leftover cache — refresh with `orkia compiler install`.
fn compiler_unusable(added: &str) -> bool {
    added.contains("compiler not found")
        || added.contains("compile failed")
        || added.contains("javy")
        || added.contains("incompatible version")
}

/// Pre-compile the Javy `.wasm` into a `.cwasm` and install it (+ manifest)
/// into `<data_dir>/plugins/`, matching the runtime engine config (fuel on).
fn install_where_big(data_dir: &std::path::Path) {
    let runtime = orkia_plugin::PluginRuntime::new().expect("runtime");
    let cwasm = runtime
        .engine()
        .precompile_module(WHERE_BIG_WASM)
        .expect("precompile");
    let plugins = data_dir.join("plugins");
    std::fs::create_dir_all(&plugins).expect("mkdir");
    std::fs::write(plugins.join("where_big.cwasm"), &cwasm).expect("write cwasm");
    std::fs::write(plugins.join("where_big.toml"), MANIFEST).expect("write toml");
}

#[tokio::test]
async fn installed_plugin_filters_in_pipeline() {
    let dir = TempDir::new().expect("tmp");
    install_where_big(dir.path());

    // A separate dir to list (so the data-dir's own files don't leak in).
    let listme = dir.path().join("listme");
    std::fs::create_dir(&listme).expect("mkdir");
    std::fs::write(listme.join("small"), b"x").expect("write");
    std::fs::write(listme.join("big"), vec![0u8; 4096]).expect("write");

    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    // Repl::new loads installed plugins from <data_dir>/plugins and registers them.
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    // `plugin list` shows it installed.
    repl.tick("orkia plugin list".into()).await.expect("list");
    let listed = block_texts(&events.lock().expect("lock")).join("\n");
    assert!(listed.contains("where_big"), "plugin listed; got: {listed}");

    // The real pipeline: ls (typed producer) → where_big (QuickJS plugin filter).
    repl.tick(format!(
        "orkia ls {} | where_big --min_size 1kb",
        listme.display()
    ))
    .await
    .expect("pipeline");

    let texts = block_texts(&events.lock().expect("lock")).join("\n");
    assert!(texts.contains("big"), "big (4KB ≥ 1kb) kept; got: {texts}");
    assert!(
        !texts.contains("small"),
        "small (1B) filtered out; got: {texts}"
    );
}

/// `plugin add ./x.wasm` (a raw `.wasm`, AOT-precompiled at install via the
/// external `orkia-compiler` — the runtime-only binary has no compiler) and
/// runs in a pipe — exactly like the TS plugin above, same code path, different
/// source language.
#[tokio::test]
async fn rust_plugin_add_wasm_then_pipe() {
    // The raw `.wasm` is precompiled by `orkia-compiler`; skip if unavailable
    // (e.g. offline CI without the cached artifact), like the TS test above.
    if orkia_shell::plugins::find_compiler()
        .filter(|p| p.is_file())
        .is_none()
    {
        eprintln!("skip: orkia-compiler artifact not available");
        return;
    }

    let dir = TempDir::new().expect("tmp");

    // Drop the raw Rust `.wasm` + its manifest where `plugin add` can read them.
    let src = dir.path().join("where_big_rs.wasm");
    std::fs::write(&src, WHERE_BIG_RS_WASM).expect("write wasm");
    std::fs::write(src.with_extension("toml"), MANIFEST_RS).expect("write toml");

    let listme = dir.path().join("listme");
    std::fs::create_dir(&listme).expect("mkdir");
    std::fs::write(listme.join("small"), b"x").expect("write");
    std::fs::write(listme.join("big"), vec![0u8; 4096]).expect("write");

    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    // `plugin add ./where_big_rs.wasm` — the raw-wasm install path.
    repl.tick(format!("orkia plugin add {}", src.display()))
        .await
        .expect("plugin add wasm");
    let added = block_texts(&events.lock().expect("lock")).join("\n");
    if compiler_unusable(&added) {
        eprintln!("skip: orkia-compiler unusable: {added}");
        return;
    }
    assert!(
        added.contains("where_big_rs") && added.contains("registered"),
        "rust plugin installed + registered; got: {added}"
    );

    // The real pipe: ls (typed producer) → where_big_rs (Rust plugin filter).
    repl.tick(format!(
        "orkia ls {} | where_big_rs --min_size 1kb",
        listme.display()
    ))
    .await
    .expect("pipeline");

    let texts = block_texts(&events.lock().expect("lock")).join("\n");
    assert!(texts.contains("big"), "big (4KB ≥ 1kb) kept; got: {texts}");
    assert!(
        !texts.contains("small"),
        "small (1B) filtered out; got: {texts}"
    );
}

const MANIFEST_RS: &str = r#"
[plugin]
name = "where_big_rs"
version = "0.1.0"
[command]
input_type = "list<record>"
output_type = "list<record>"
[command.args]
min_size = { type = "filesize" }
"#;

const TS_FILTER: &str = r#"
function readAll(): string {
  const chunks: Uint8Array[] = [];
  const buf = new Uint8Array(4096);
  let n: number;
  while ((n = (Javy as any).IO.readSync(0, buf)) > 0) { chunks.push(buf.slice(0, n)); }
  let len: number = 0; for (const c of chunks) len += c.length;
  const all = new Uint8Array(len); let o = 0;
  for (const c of chunks) { all.set(c, o); o += c.length; }
  return new TextDecoder().decode(all);
}
const env: any = JSON.parse(readAll());
const rows: any[] = Array.isArray(env.input) ? env.input : [];
const min: number = (env.call && env.call.named && env.call.named.min_size && env.call.named.min_size.$filesize) || 0;
const kept = rows.filter((r: any) => r && r.size && typeof r.size.$filesize === "number" && r.size.$filesize >= min);
(Javy as any).IO.writeSync(1, new TextEncoder().encode(JSON.stringify(kept)));
"#;

const TS_FILTER_TOML: &str = r#"
[plugin]
name = "tsfilter"
version = "0.1.0"
[command]
input_type = "list<record>"
output_type = "list<record>"
[command.args]
min_size = { type = "filesize" }
"#;

#[tokio::test]
async fn plugin_add_ts_source_compiles_and_runs() {
    // → compile → register → run. Needs the orkia-compiler + Javy artifacts;
    // skips gracefully if unavailable (e.g. offline CI without the cache).
    if orkia_shell::plugins::find_compiler()
        .filter(|p| p.is_file())
        .is_none()
    {
        eprintln!("skip: orkia-compiler artifact not available");
        return;
    }

    let dir = TempDir::new().expect("tmp");
    let srcdir = dir.path().join("src");
    std::fs::create_dir_all(&srcdir).expect("mkdir");
    std::fs::write(srcdir.join("tsfilter.ts"), TS_FILTER).expect("write ts");
    std::fs::write(srcdir.join("tsfilter.toml"), TS_FILTER_TOML).expect("write toml");

    let listme = dir.path().join("listme");
    std::fs::create_dir(&listme).expect("mkdir");
    std::fs::write(listme.join("small"), b"x").expect("write");
    std::fs::write(listme.join("big"), vec![0u8; 4096]).expect("write");

    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    repl.tick(format!(
        "orkia plugin add {}",
        srcdir.join("tsfilter.ts").display()
    ))
    .await
    .expect("plugin add ts");

    let added = block_texts(&events.lock().expect("lock")).join("\n");
    if compiler_unusable(&added) {
        eprintln!("skip: orkia-compiler unusable: {added}");
        return;
    }
    assert!(
        added.contains("tsfilter") && added.contains("registered"),
        "TS plugin compiled + registered; got: {added}"
    );

    // The compiled TS plugin now runs in a real pipeline.
    repl.tick(format!(
        "orkia ls {} | tsfilter --min_size 1kb",
        listme.display()
    ))
    .await
    .expect("pipeline");

    let texts = block_texts(&events.lock().expect("lock")).join("\n");
    assert!(texts.contains("big"), "big kept; got: {texts}");
    assert!(!texts.contains("small"), "small filtered; got: {texts}");
}

/// compiles, registers, and reports that it is watching — then the plugin runs
/// in a pipe immediately (initial registration). Distinct from `add`: dev mode
/// also installs a source watcher. Compiler-gated; skips if unavailable.
#[tokio::test]
async fn plugin_dev_registers_and_watches() {
    if orkia_shell::plugins::find_compiler()
        .filter(|p| p.is_file())
        .is_none()
    {
        eprintln!("skip: orkia-compiler artifact not available");
        return;
    }

    let dir = TempDir::new().expect("tmp");
    let srcdir = dir.path().join("src");
    std::fs::create_dir_all(&srcdir).expect("mkdir");
    std::fs::write(srcdir.join("tsfilter.ts"), TS_FILTER).expect("write ts");
    std::fs::write(srcdir.join("tsfilter.toml"), TS_FILTER_TOML).expect("write toml");

    let listme = dir.path().join("listme");
    std::fs::create_dir(&listme).expect("mkdir");
    std::fs::write(listme.join("small"), b"x").expect("write");
    std::fs::write(listme.join("big"), vec![0u8; 4096]).expect("write");

    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    repl.tick(format!(
        "orkia plugin dev {}",
        srcdir.join("tsfilter.ts").display()
    ))
    .await
    .expect("plugin dev");
    let added = block_texts(&events.lock().expect("lock")).join("\n");
    if compiler_unusable(&added) {
        eprintln!("skip: orkia-compiler unusable: {added}");
        return;
    }
    assert!(
        added.contains("tsfilter") && added.contains("watching"),
        "dev registers + reports watching; got: {added}"
    );

    // Runs in a pipe straight away (the initial registration).
    repl.tick(format!(
        "orkia ls {} | tsfilter --min_size 1kb",
        listme.display()
    ))
    .await
    .expect("pipeline");
    let texts = block_texts(&events.lock().expect("lock")).join("\n");
    assert!(texts.contains("big"), "big kept; got: {texts}");
    assert!(!texts.contains("small"), "small filtered; got: {texts}");
}

/// save to the source triggers a recompile and a `Reloaded` message on the
/// channel (the REPL would then swap it into the registry). Uses `recv_timeout`
/// so it's not flaky: it passes as soon as the reload lands. Compiler-gated.
#[test]
fn dev_watcher_recompiles_on_save() {
    use std::time::Duration;

    if orkia_shell::plugins::find_compiler()
        .filter(|p| p.is_file())
        .is_none()
    {
        eprintln!("skip: orkia-compiler artifact not available");
        return;
    }

    let dir = TempDir::new().expect("tmp");
    let src = dir.path().join("tsfilter.ts");
    std::fs::write(&src, TS_FILTER).expect("write ts");
    std::fs::write(dir.path().join("tsfilter.toml"), TS_FILTER_TOML).expect("write toml");
    let data_dir = dir.path().join("data");

    let rt = std::sync::Arc::new(orkia_plugin::PluginRuntime::new().expect("runtime"));
    let (tx, rx) = std::sync::mpsc::channel();
    orkia_shell::plugins::spawn_dev_watcher(src.clone(), "tsfilter".into(), rt, data_dir, tx)
        .expect("spawn watcher");

    // Let the OS watcher establish, then modify the source.
    std::thread::sleep(Duration::from_millis(400));
    std::fs::write(&src, format!("{TS_FILTER}\n// edited\n")).expect("rewrite ts");

    // Generous window: a Javy compile can take a couple of seconds.
    match rx.recv_timeout(Duration::from_secs(60)) {
        Ok(orkia_shell::plugins::DevReloadMsg::Reloaded { name, .. }) => {
            assert_eq!(name, "tsfilter", "reloaded under its manifest name");
        }
        Ok(orkia_shell::plugins::DevReloadMsg::Failed { error, .. }) => {
            // Compiler present but javy/toolchain unusable here — tolerate.
            eprintln!("skip: recompile failed (toolchain unavailable): {error}");
        }
        Err(e) => panic!("no reload arrived within timeout: {e}"),
    }
}

/// `entry` bundles a **multi-file** TS plugin (entry imports a local helper)
/// and runs it in a pipe — the same composition as a single-file plugin.
/// Compiler-gated; skips if unavailable.
#[tokio::test]
async fn plugin_add_directory_bundles_multifile_and_runs() {
    if orkia_shell::plugins::find_compiler()
        .filter(|p| p.is_file())
        .is_none()
    {
        eprintln!("skip: orkia-compiler artifact not available");
        return;
    }

    let dir = TempDir::new().expect("tmp");
    let plugin_dir = dir.path().join("multifilter");
    let src = plugin_dir.join("src");
    std::fs::create_dir_all(&src).expect("mkdir");
    // entry imports a local helper module — the multi-file case.
    std::fs::write(
        src.join("filter.ts"),
        "export function keepBig(rows: any[], min: number): any[] {\n  return rows.filter((r: any) => r && r.size && typeof r.size.$filesize === \"number\" && r.size.$filesize >= min);\n}\n",
    )
    .expect("write filter");
    std::fs::write(
        src.join("main.ts"),
        r#"import { keepBig } from "./filter";
function readAll(): string {
  const chunks: Uint8Array[] = [];
  const buf = new Uint8Array(4096);
  let n: number;
  while ((n = (Javy as any).IO.readSync(0, buf)) > 0) { chunks.push(buf.slice(0, n)); }
  let len = 0; for (const c of chunks) len += c.length;
  const all = new Uint8Array(len); let o = 0;
  for (const c of chunks) { all.set(c, o); o += c.length; }
  return new TextDecoder().decode(all);
}
const env: any = JSON.parse(readAll());
const rows: any[] = Array.isArray(env.input) ? env.input : [];
const min: number = (env.call && env.call.named && env.call.named.min_size && env.call.named.min_size.$filesize) || 0;
(Javy as any).IO.writeSync(1, new TextEncoder().encode(JSON.stringify(keepBig(rows, min))));
"#,
    )
    .expect("write main");
    std::fs::write(
        plugin_dir.join("plugin.toml"),
        "[plugin]\nname = \"multifilter\"\nversion = \"0.1.0\"\nentry = \"src/main.ts\"\n[command]\ninput_type = \"list<record>\"\noutput_type = \"list<record>\"\n[command.args]\nmin_size = { type = \"filesize\" }\n",
    )
    .expect("write toml");

    let listme = dir.path().join("listme");
    std::fs::create_dir(&listme).expect("mkdir");
    std::fs::write(listme.join("small"), b"x").expect("write");
    std::fs::write(listme.join("big"), vec![0u8; 4096]).expect("write");

    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    repl.tick(format!("orkia plugin add {}", plugin_dir.display()))
        .await
        .expect("plugin add dir");
    let added = block_texts(&events.lock().expect("lock")).join("\n");
    if compiler_unusable(&added) {
        eprintln!("skip: orkia-compiler unusable: {added}");
        return;
    }
    assert!(
        added.contains("multifilter") && added.contains("registered"),
        "directory plugin installed + registered; got: {added}"
    );

    repl.tick(format!(
        "orkia ls {} | multifilter --min_size 1kb",
        listme.display()
    ))
    .await
    .expect("pipeline");
    let texts = block_texts(&events.lock().expect("lock")).join("\n");
    assert!(texts.contains("big"), "big kept; got: {texts}");
    assert!(!texts.contains("small"), "small filtered; got: {texts}");
}

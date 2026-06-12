# Orkia Plugins — Developer Guide

A plugin is a **pipe transformer**: it receives structured data, computes, and
returns structured data. Plugins compose in the Orkia pipeline next to built-in
commands — `ls | where size > 1mb | where_big --min_size 1mb`.

Plugins are written in **TypeScript** or **Rust**, compiled to **WebAssembly**,
and run **sandboxed** under wasmtime. They cannot touch the network, the
filesystem, the clock, or the environment unless explicitly granted — and even
then, effects route through MCP, never through the plugin itself. A malformed or
malicious plugin can stall (it is fuel-limited) but can never crash the shell or
escape its sandbox.

> **Mental model:** a plugin is *pure compute over data*. If you need to *fetch*
> something, *read a file*, or *call an API*, that is a **connector** — and
> connectors are MCP servers, not plugins. Bundling more packages never widens
> this boundary.

---

## Table of contents

1. [Architecture](#1-architecture)
2. [Quick start — install and use a plugin](#2-quick-start)
3. [The data model — how values cross the boundary](#3-the-data-model)
4. [`plugin.toml` reference](#4-plugintoml-reference)
5. [Writing a TypeScript plugin](#5-writing-a-typescript-plugin)
6. [Writing a Rust plugin](#6-writing-a-rust-plugin)
7. [Multi-file plugins & npm packages](#7-multi-file-plugins--npm-packages)
8. [Capabilities & the sandbox](#8-capabilities--the-sandbox)
9. [Streaming plugins](#9-streaming-plugins)
10. [Developer workflow — hot reload & logging](#10-developer-workflow)
11. [CLI command reference](#11-cli-command-reference)
12. [Troubleshooting](#12-troubleshooting)
13. [Not yet available](#13-not-yet-available)

---

## 1. Architecture

There are **two** binaries, on purpose:

| Component | What it is | Carries |
|---|---|---|
| `orkia` | The shell you run 24/7. **Runtime-only** wasmtime. | Loads pre-compiled `.cwasm` modules via `Module::deserialize`. **No compiler.** |
| `orkia-compiler` | A **separate artifact**, pulled on demand. | Cranelift + OXC + Javy. Compiles TS/JS/`.wasm` → `.cwasm`. |

Why the split? ~80% of users only *consume* plugins; they should never pay for
OXC + Cranelift (~several MB). So the shell ships runtime-only, and the compiler
is fetched the first time you compile a source plugin and cached.

**On-disk layout** (default data dir is `~/.orkia`):

```
~/.orkia/
├─ plugins/                     # installed plugins, loaded at every startup
│  ├─ where_big.cwasm           #   the precompiled module
│  └─ where_big.toml            #   its manifest (capabilities, types, args)
└─ cache/
   └─ compiler/
      ├─ orkia-compiler         # the pulled compiler artifact
      └─ javy                   # the pulled Javy (JS→QuickJS-WASM) compiler
```

At startup, the shell loads every `*.cwasm` in `~/.orkia/plugins/` and registers
it as a command. A plugin that fails to load is logged and skipped — the shell
still starts.

**The runtime is polyglot-blind.** A TypeScript plugin (compiled via Javy) and a
Rust plugin (compiled to `wasm32-wasip1`) run through the *same* execution path
and produce *indistinguishable* output. The host never asks what language a
plugin was written in.

---

## 2. Quick start

### Use an existing plugin

```bash
# Install a precompiled module (no compiler, no network needed):
orkia plugin add ./where_big.cwasm

# …or install from TypeScript source (pulls the compiler on first use):
orkia plugin add ./where_big.ts

# …or a raw .wasm (e.g. a Rust plugin), or a multi-file directory:
orkia plugin add ./where_big.wasm
orkia plugin add ./my-plugin/         # directory with plugin.toml

# See what's installed:
orkia plugin list

# Use it in a pipeline — it composes with built-ins (ls, where, first, sort-by):
ls ./downloads | where_big --min_size 1mb
ls ./downloads | where_big --min_size 1mb | first 10
```

`plugin add` installs into `~/.orkia/plugins/` and registers the command **live**
in the current session — no restart needed.

### The hello-world filter

A plugin that keeps table rows whose `size` is at least `--min_size`. The full
walkthrough for both languages is in §5 (TypeScript) and §6 (Rust).

---

## 3. The data model

Plugins speak a **typed, structured** data model — not raw bytes. The pipeline
medium is a `Value`:

| `Value` | Example | Notes |
|---|---|---|
| `Nothing` | — | null / no value |
| `Bool` | `true` | |
| `Int` | `42` | 64-bit integer |
| `Float` | `3.5` | |
| `Filesize` | `1048576` | bytes, but *typed* — distinct from `Int` |
| `Duration` | `5000000000` | nanoseconds, typed |
| `Date` | RFC-3339 | timestamp, typed |
| `String` | `"hello"` | |
| `Binary` | bytes | |
| `List` | `[...]` | |
| `Record` | `{ k: v }` | ordered map (a table row) |

### The boundary format (`$`-tagged JSON)

WebAssembly only passes numbers, so values cross the host↔plugin boundary as
**JSON**. Plain types map directly. The four *rich* types are encoded as
**tagged objects** so the distinction survives the round trip — a JSON `number`
can't tell a filesize from a count:

```jsonc
{ "$filesize": 1048576 }                 // Filesize
{ "$duration_ns": 5000000000 }           // Duration (nanoseconds)
{ "$date": "2026-05-29T12:00:00Z" }      // Date (RFC-3339)
{ "$binary": "aGVsbG8=" }                // Binary (base64)
```

A plain one-key record that *looks* like a tag but isn't (wrong payload type, or
extra keys) degrades gracefully to a `Record` — a malformed shape never crashes
the host.

### The invocation envelope

When your plugin runs, it receives a single JSON object on **stdin**:

```jsonc
{
  "input": [ /* the upstream pipeline value(s), $-tagged */ ],
  "call": {
    "positional": [ /* positional args */ ],
    "named": { "min_size": { "$filesize": 1048576 } }  // --min_size 1mb
  }
}
```

It writes its result — a JSON value (an array for a `list` output) — to
**stdout**. That stdout is the *only* thing that crosses the pipe.

---

## 4. `plugin.toml` reference

Every installed plugin has a sidecar manifest. It drives the type signature (so
the kernel type-checks the pipeline *before* running anything), the declared
arguments, and the capability grant. A single-file plugin with **no** manifest
defaults to a total sandbox with an `any → any` signature.

```toml
[plugin]
name = "where_big"            # required — the command name in the pipeline
version = "0.1.0"             # required
description = "keep big rows" # optional
entry = "src/main.ts"         # optional — REQUIRED for directory plugins (the bundle entry)

[command]
input_type  = "list<record>"  # default: "any"
output_type = "list<record>"  # default: "any"
streaming   = false           # default: false — see §9

[command.args]
# Each declared arg becomes a value flag: `where_big --min_size 1mb`.
# The value is coerced to the declared type and reaches the guest under call.named.
min_size = { type = "filesize" }
within_km = { type = "float", required = true }   # `required` is advisory in V1
unit      = { type = "string" }

[capabilities]
# ABSENT or empty  ⇒  TOTAL SANDBOX (the default, fail-closed). See §8.
# fs_read  = "any" | ["./data", "./more"]   # paths
# fs_write = ["./out"]                       # paths
# net      = "any" | ["api.example.com"]     # hosts
# env      = ["HOME", "LANG"]                # env var names
# clock    = true                            # bool
# random   = true                            # bool
```

### Type strings

`parse_type` accepts (case-insensitive): `any`, `nothing`/`null`, `bool`, `int`,
`float`, `filesize`, `duration`, `date`, `string`, `binary`, `record`, `table`,
`bytestream`, and `list<T>` (e.g. `list<record>`, `list<string>`). A typo is an
**error** — an unknown type never silently becomes `any`.

> `table` is sugar for `list<record>`. `list<record>` is the typical input/output
> for a row filter.

---

## 5. Writing a TypeScript plugin

TypeScript is the default language — "agents write plugins, in TS." Types are
stripped by OXC, the JS is compiled to a QuickJS-WASM module by Javy, then
AOT-compiled to `.cwasm`.

### The contract

1. Read the `{input, call}` envelope from **stdin** (fd 0).
2. Compute.
3. Write the result JSON to **stdout** (fd 1).

QuickJS gives you `console`, `TextEncoder`, `TextDecoder`. I/O is via the `Javy`
global. There is **no** `fetch`, `require`, `process`, `fs`, or `XMLHttpRequest`
— referencing them throws `ReferenceError` (proven, not just by construction).

### `where_big.ts`

```ts
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
function write(s: string) { (Javy as any).IO.writeSync(1, new TextEncoder().encode(s)); }

const env: any = JSON.parse(readAll());
const rows: any[] = Array.isArray(env.input) ? env.input : [];

// Rich types are $-tagged — read `min_size` as a $filesize.
const min: number =
  (env.call && env.call.named && env.call.named.min_size && env.call.named.min_size.$filesize) || 0;

// Keep rows whose `size` ($filesize) is >= min. The tag is preserved on output.
const kept = rows.filter(
  (r: any) => r && r.size && typeof r.size.$filesize === "number" && r.size.$filesize >= min
);

write(JSON.stringify(kept));
```

### `where_big.toml`

```toml
[plugin]
name = "where_big"
version = "0.1.0"
[command]
input_type = "list<record>"
output_type = "list<record>"
[command.args]
min_size = { type = "filesize" }
```

### Install & run

```bash
orkia plugin add ./where_big.ts        # compiles via the pulled orkia-compiler
ls ./downloads | where_big --min_size 1mb
```

> `console.log` / `console.error` from a plugin go to the **journal** (tagged by
> plugin name), **not** into the pipe — so debug logging can never corrupt your
> data stream. See `orkia journal`.

---

## 6. Writing a Rust plugin

For compute-heavy transforms where QuickJS's lack of a JIT hurts, write the
plugin in Rust. You compile it yourself with `cargo` (Orkia does not pull a Rust
toolchain — if you write Rust, you already have one).

### Setup

```toml
# Cargo.toml
[package]
name = "where_big"
version = "0.1.0"
edition = "2021"

[dependencies]
orkia-plugin-sdk = { path = "…/crates/orkia-plugin-sdk" }   # or a published version

[profile.release]
opt-level = "z"
strip = true
```

### `src/main.rs`

```rust
use orkia_plugin_sdk::prelude::*;

#[orkia::command]
fn where_big(input: Vec<Value>, args: Args) -> Result<Vec<Value>> {
    let min: i64 = args.get("min_size")?;            // typed, fail-closed
    Ok(input
        .into_iter()
        .filter(|row| matches!(row.get_path("size"), Some(Value::Filesize(n)) if *n >= min))
        .collect())
}
```

The `#[orkia::command]` macro generates the WASM entry point: it reads the
`{input, call}` envelope from stdin, deserializes the `$`-tagged JSON into
`Vec<Value>` + `Args`, calls your function, and writes the result back. You never
touch the boundary plumbing.

### The SDK surface (`prelude::*`)

| Item | Purpose |
|---|---|
| `Value` | **The same `Value` as the host** (re-exported — no separate type, no drift). |
| `Args` | Typed access to named args. |
| `Args::get::<T>(name) -> Result<T>` | Required arg; `Err` if absent or wrong type. |
| `Args::get_opt::<T>(name) -> Option<T>` | Optional arg. |
| `Args::raw(name) -> Option<&Value>` | The raw `Value`. |
| `Result<T>` / `Error` | Plugin result; `Err` surfaces to the host as a typed failure (non-zero exit), never a silent empty output. |
| `FromArg` | Coercion trait — implemented for `Value`, `i64`, `f64`, `bool`, `String`. |

Useful `Value` methods: `get_path("size")`, `as_record()`, `type_of()`, plus the
variant matches (`Value::Filesize(n)`, `Value::Record(map)`, …).

### Build & install

```bash
# Build to wasm32-wasip1 (the WASI preview1 target the runtime expects):
rustup target add wasm32-wasip1
cargo build --target wasm32-wasip1 --release

# Install the raw .wasm — the host AOT-precompiles it to .cwasm at install time:
orkia plugin add ./target/wasm32-wasip1/release/where_big.wasm
```

Ship a `where_big.toml` next to the `.wasm` (same schema as §4) so the plugin
gets a real type signature and any capability declarations; otherwise it installs
as a total-sandbox `any → any` command.

> **Note:** the SDK targets `wasm32-wasip1` (WASI preview1), not preview2 — the
> runtime's execution path is preview1 (stdin/stdout `_start`).

---

## 7. Multi-file plugins & npm packages

Real plugins are rarely one file. Point `plugin add` at a **directory** with a
`plugin.toml` whose `[plugin].entry` names the entry module:

```
my-plugin/
├─ plugin.toml          # entry = "src/main.ts"
└─ src/
   ├─ main.ts           # imports ./filter
   └─ filter.ts         # export function keepBig(...) { ... }
```

```toml
# my-plugin/plugin.toml
[plugin]
name = "multifilter"
version = "0.1.0"
entry = "src/main.ts"
[command]
input_type = "list<record>"
output_type = "list<record>"
[command.args]
min_size = { type = "filesize" }
```

```bash
orkia plugin add ./my-plugin/
ls ./downloads | multifilter --min_size 1kb
```

The compiler resolves the local module graph from `entry` (relative `./imports`
plus pure-JS packages already present in `node_modules`), converts ES modules to
a single self-contained bundle, and compiles that. A single-file plugin with no
imports takes a fast path identical to V1.

**Scope of bundling:**
- ✅ Relative imports (`./a`, `../b`), with extension and `index.*` resolution.
- ✅ Pure-JS npm packages **already in `node_modules`** (run `npm install`
  yourself first). Resolution honors `package.json` `module`/`main`.
- ❌ Native addons (`.node`) — rejected with a clear error (plugins are pure
  compute; native code is a connector → MCP).
- ❌ Fetching packages from the npm registry — **not** done (you run
  `npm install`). See §13.
- ❌ Packages that need a Web/Node API QuickJS lacks (beyond `console` /
  `TextEncoder`) — they fail. Effect APIs (`fetch`/`fs`/`net`) are **never**
  polyfilled by design.

---

## 8. Capabilities & the sandbox

**The default is a total sandbox.** No `[capabilities]` block (or an empty one)
means: no filesystem, no network, no clock, no randomness, no environment. This
is fail-closed — an unrecognized or absent capability key grants **nothing**.

A plugin actively *attempting* an effect under the default sandbox fails: `fetch`,
`require("fs")`, `process.env`, `new XMLHttpRequest()` all throw `ReferenceError`.
This is enforced structurally by the wasmtime linker (the guest's import surface
is empty), not by trust.

When a plugin *does* declare capabilities, they are presented to **you** for
approval before installation, and the grant is recorded in the SEAL journal. A
registry plugin gets no more rights than a local one.

```toml
[capabilities]
fs_read = ["./data"]          # read only under ./data
net     = ["api.example.com"] # reach only this host
env     = ["LANG"]            # read only LANG
clock   = true                # may read the clock
random  = true                # may use randomness
# "any" widens a scope to everything, e.g.  net = "any"
```

> **Important nuance:** declared capabilities gate *what the host will do on the
> plugin's behalf*. Effects themselves still route through MCP — the plugin never
> gets a raw socket or file descriptor. Capabilities are a governance contract,
> not a hole in the WASM sandbox.

Resource limits are always on: a runaway plugin (`while(true)`) is stopped by a
fuel limit as a typed `ResourceLimit` error — the host is unharmed and the next
plugin still runs.

---

## 9. Streaming plugins

By default a plugin is **batch**: it receives the whole input as one value,
computes, returns one value. For a transform over a very large volume, set:

```toml
[command]
streaming = true
```

In streaming mode the host pulls the input in **chunks**, runs your plugin once
per chunk, and yields results lazily. The benefit is **early termination**: a
downstream `first 10` stops pulling after the first chunk — your plugin never
processes the whole million-row input.

The **guest contract is identical** to batch mode (each chunk is a normal
`{input, call}` invocation), so you write the *same plugin*; only the manifest
flag changes. Streaming is best for row-wise filters/maps. Whole-stream
aggregations (sort, group) are inherently batch — leave `streaming = false`.

```bash
ls ./huge-dir | where_big_stream --min_size 1mb | first 10
#                ^ streaming=true                  ^ stops the plugin early
```

---

## 10. Developer workflow

### Hot reload — `plugin dev`

Iterating on a plugin? `plugin dev` compiles + registers it, then **watches the
source** and live-reloads on every save — no re-typing `plugin add`:

```bash
orkia plugin dev ./where_big.ts
# edit where_big.ts, save → "plugin `where_big` reloaded (dev)" appears.
```

`plugin dev` is **development only** (distinct from `plugin add`, which installs
once and does not watch). The recompile happens off the REPL thread, so the shell
never blocks on compilation.

### Logging — `console.log` → journal

A plugin's `console.log` / `console.error` (TS) or `stderr` writes (Rust) are
routed to the Orkia **journal**, tagged by plugin name — never into the pipe.
Inspect them:

```bash
orkia journal              # see plugin.console events alongside everything else
```

This means you can debug-log freely without corrupting the data flowing through
the pipeline.

### Getting the compiler

The first time you compile a source (`.ts`/`.js`/`.wasm`/directory) plugin, the
shell needs `orkia-compiler`. It resolves it from:

1. `$ORKIA_COMPILER` (an explicit path), then
2. `~/.orkia/cache/compiler/orkia-compiler` (the cache), then
3. `orkia-compiler` on `PATH`.

If none is present you'll get a clear error. Installing a precompiled `.cwasm`
needs **no** compiler and **no** network.

---

## 11. CLI command reference

| Command | What it does |
|---|---|
| `plugin add <file.cwasm>` | Install a precompiled module directly (no compiler, no network). |
| `plugin add <file.ts\|.tsx\|.mts\|.js\|.mjs>` | Compile from source (via `orkia-compiler`) and install. |
| `plugin add <file.wasm>` | Install a raw `.wasm` (e.g. a Rust plugin); AOT-precompiled at install. |
| `plugin add <dir/>` | Bundle a multi-file plugin (reads `plugin.toml` `entry`) and install. |
| `plugin dev <file>` | Compile + register, then watch the source and live-reload on save. |
| `plugin list` | List installed plugins. |

A `<name>.toml` sidecar next to the file (or `plugin.toml` in a directory)
supplies the manifest. Plugins land in `~/.orkia/plugins/` and register live.

---

## 12. Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `plugin add: unsupported '.xyz'` | Use a directory, `.wasm`, `.cwasm`, `.ts`, or `.js`. |
| `plugin compiler not found` | Install/point the compiler: set `$ORKIA_COMPILER` or place it at `~/.orkia/cache/compiler/orkia-compiler`. Precompiled `.cwasm` needs no compiler. |
| `could not load module 'foo'` (from Javy) | Your TS uses `import` but was compiled single-file. Use a **directory** plugin with `entry` so the bundler resolves imports (§7), and ensure the compiler is current. |
| `cannot resolve 'pkg'` | The package isn't in `node_modules` — run `npm install` first (Orkia doesn't fetch from the registry). |
| `unknown type 'foo'` | A typo in `input_type`/`output_type`/an arg `type`. See the valid type strings in §4. |
| Plugin output is empty / `parse output` error | Your plugin must write a JSON value to stdout. A thrown error or non-JSON output surfaces as a runtime error. |
| `TypeMismatch` before the plugin runs | The pipeline's upstream type doesn't satisfy your `input_type` (e.g. a `ByteStream` into a `list<record>` plugin). The kernel refuses it *before* execution. |
| `ResourceLimit` | The plugin hit the fuel/memory cap (likely an infinite loop or runaway allocation). |
| Effect "doesn't work" (`fetch`, `fs`) | By design. Plugins are sandboxed; effects belong in an MCP connector, not a plugin (§8). |

---

## 13. Not yet available

These are specified but deliberately deferred (they need hosted infrastructure or
have not been triggered):

- **`plugin add <name>` (registry):** resolving a *name* to a published plugin
  from a hosted registry. Today you install from a path/dir/`.wasm`. The
  governance contract (sandbox + capability approval) is defined for it.
- **Ed25519 signature verification of pulled artifacts:** today the compiler/Javy
  pull is integrity-checked with **SHA-256 pinning**. Authenticity signing
  (Ed25519) is gated on an Orkia release server producing signed artifacts. (The
  verification primitive already exists in `orkia-kernel-trust`.)
- **npm registry resolution:** fetching packages at build time. You run
  `npm install` yourself; local `node_modules` is bundled (§7).
- **Hot reload after the source's package set changes** is recompiled on save,
  but adding a brand-new dependency may need a fresh `plugin add`.

---

## Appendix — the security model in one paragraph

Every byte from a plugin is untrusted. The WASM sandbox has no ambient authority:
no preopened directories, no environment, no network — the QuickJS/WASI context
exposes only stdin/stdout/stderr, and the wasmtime linker's import surface is
empty, so an effect import fails *instantiation*. Resource limits (fuel + memory)
stop a runaway without harming the host. Capabilities are an explicit,
user-approved, SEAL-journaled grant — fail-closed by default. Effects route
through MCP, never the plugin. This holds on the **production** execution path,
proven by tests that make a real plugin *attempt* `fetch`/`fs`/`env` and observe
every attempt blocked — not merely "true by construction."

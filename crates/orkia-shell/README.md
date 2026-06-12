# orkia-shell

The agentic shell core: REPL, decision engine, classifier and router
implementations, brush-backed shell engine, SEAL emitter, job
controller, approval watcher, journal store, and configuration.

## Overview

`orkia-shell` is the heart of the Orkia shell. It owns the REPL main
loop, the in-process shell engine (a fork of `brush`), the job table,
the SEAL chain, the journal, and the rendering pipeline. Everything
in this crate sits behind traits defined in `orkia-shell-types`, so
the same `Repl<C, A>` can drive stdout-mode, the ratatui TUI, or a
test harness with no changes.

The REPL loop is the heartbeat of the shell. It reads a line from
the active renderer, classifies it (`Shell`, `Builtin`, `Agent`,
`Pipeline`, `NoOp`), dispatches to the right subsystem, drains any
queued events (job updates, approvals, journal envelopes, workspace
mutations), renders notifications, and loops. The loop never blocks
on anything but user input; PTY writes, journal I/O, brush
execution, and approval polling all happen on dedicated tasks.

The crate is intentionally single-owner: PTY master fds live with
the job's reader thread; the journal store owns its own task; the
SEAL chain mutates only on the REPL thread; the brush session is
held exclusively by the REPL. Sharing across subsystems happens via
channels and snapshots, not `Arc<Mutex<T>>` on hot data.

## Key modules

- `repl` — `Repl<C, A>` and its `run()` loop. Holds the renderer,
  classifier, router, job controller, SEAL state, journal handles,
  approval watcher, workspace cache, brush session.
- `engine` — `ShellEngine` and `BrushSession`: the in-process
  bash-compatible engine (forked `brush-core`) that executes shell
  commands without spawning a child shell. `CommandOutput`,
  `ExecuteResult`, `ShellEngineOptions`.
- `classifier` — `HeuristicClassifier`, `resolve_mode`, and the
  builtin name tables (`AUGMENTED_BUILTINS`, `AGENTIC_BUILTINS`).
  Stateless; brush returns 127 for unknown commands so the
  classifier only has to disambiguate `Command` vs `Agent` for
  ambiguous input.
- `router` — `HeuristicRouter` implementing `AgentRouter`: picks an
  agent by archetype match, explicit `@name` prefix, single-agent
  shortcut, or default.
- `decision` — re-exports of the shell-types `Decision` /
  `BuiltinCmd` / `Mode` plus local extensions.
- `pipeline` — `parse_pipeline` for the `|>` agent pipeline syntax.
- `job` — `JobController` and `foreground` (the attached-job
  registry consulted by the signal-forwarding code in the binary).
- `seal` — `SealManager`, `SealChain`, the `audit` builtin renderer, JSON
  envelope format. SHA-256 chain over shell events with project-scoped
  journals.
- `journal` — `JournalStore`, `JournalListener`, `JournalEnvelope`,
  filter parsing. Backs `~/.orkia/journal.jsonl` and the Unix
  socket `~/.orkia/run/orkia.sock` agents post hooks to.
- `approval` — `ApprovalWatcher`, `ApprovalRequest`,
  `ApprovalResponse`, `PendingApproval`. Persistent file-backed
  approval queue.
- `agent`, `agent_context`, `agent_dir`, `agent_migration` —
  on-disk agent definitions, context loaders, directory layout,
  schema migrations.
- `config` — `ShellConfig` (`~/.orkia/config.toml`),
  `NotificationVerbosity`.
- `history` — persistent shell history (`~/.orkia/history`).
- `hooks`, `injection_executor`, `protocol`, `terminal_state` — the
  agent-hook bridge plumbing.
- `renderers` — `ShellModeRenderer` (rustyline-backed line editor +
  stderr toasts) and `StdoutRenderer` (minimal, used in pipes / CI).

## Public API surface

The REPL is generic over a classifier and a router and takes a boxed
renderer:

```rust
use orkia_shell::{
    HeuristicClassifier, HeuristicRouter, Repl, ShellConfig,
    ShellModeRenderer,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ShellConfig::load();
    let mut repl = Repl::new(
        ShellModeRenderer::new(),
        HeuristicClassifier,
        HeuristicRouter,
        config,
    );
    repl.run().await?;
    Ok(())
}
```

For single-shot `orkia -c "..."` invocations, drive the engine
directly without the REPL:

```rust
use orkia_shell::engine::{ShellEngine, ShellEngineOptions};

let opts = ShellEngineOptions { load_bashrc: true, load_profile: true, login: false };
let mut engine = ShellEngine::new_with_options(opts).await?;
let result = engine.execute("echo hello").await?;
assert_eq!(result.exit_code, 0);
```

## Consumed by

- `bins/orkia` — picks a renderer, constructs `Repl`, calls
  `run()`. Also calls `ShellEngine` directly for `-c` mode.
- `orkia-shell-tui` — consumes the renderer trait re-exported here
  via `orkia-shell-types`.

## Development notes

- Brush is pinned to a fork (`orkiaHQ/brush`) at a specific
  revision; bumping it is a deliberate act because brush-core's
  public API is pre-1.0.
- `rustyline` (no-default-features, with file history) backs the
  shell-mode line editor.
- The REPL keeps `unwrap_used` / `expect_used` denials. Background
  tasks return `Result` and route errors through the journal.

## License

`Elastic-2.0`; see [`../../LICENSE`](../../LICENSE).

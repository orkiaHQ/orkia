# orkia-terminal-core

The validated three-thread lock-free terminal engine — Reader,
Extractor, Render — packaged behind `TerminalEngine`. `gpui`-free
and free of user-facing strings: it exposes typed errors
(`EngineError`) and `tracing` diagnostics only.

## Overview

This crate is the terminal-rendering heart of Orkia. It owns the
PTY (via `orkia-pty`), the reader thread that drains the PTY and
advances the alacritty grid, and the extractor thread that
publishes immutable snapshots of that grid to the render side.
The render thread (the application's UI loop) takes those
snapshots by cloning an `Arc` — it never touches the grid
directly.

The threading contract is validated and frozen. The reader-thread
hot path was relocated verbatim from the original POC after
benchmarking; the `bench-terminal` harness compares against
`baselines/perf.json` and trips on regressions. **Do not move
work between threads or change synchronization primitives here
without re-running that benchmark.** This is exactly the kind of
component covered by the workspace "no band-aids on structural
problems" rule.

See `ARCHITECTURE-TERMINAL.md` (in the repo root) for the full
threading contract and the tripwire list.

## Key modules

- `engine` — `TerminalEngine`, `TerminalEngine::start(EngineConfig)`,
  `AdoptMaster`, `RawOutputRx`. Owns the PTY handles, the reader
  thread, and the extractor thread. Hands the application the
  shared dims, the screen `Term`, the wake receiver, and any
  optional OSC-133 / APC callbacks.
- `blocks` — block parser, `BlocksState`, `SharedBlocks`,
  `Osc133Marker`, `Osc133Callback`, `ApcCallback`, extractor
  thread.
- `ansi` — ANSI / VTE handling on top of `alacritty_terminal::vte`.
- `cursor` — `CursorInfo`, `CursorShape`, `extract_cursor` for
  surfacing cursor state from a snapshot.
- `state` — `StateMachine`, `SharedState`, `DisplayMode`.
- `prescan` — fast-path scan over raw PTY bytes for marker
  detection ahead of the full VTE pass.
- `render_snapshot` — the immutable grid snapshot type the
  extractor publishes and the render side clones.
- `wake` — `Wake`, `WakeRx`, `wake_pair()`. Single-consumer
  repaint signal: the extractor wakes the render loop without
  any allocation on the hot path.
- `config` — `EngineConfig`. Defaults are the POC-validated
  values; override sparingly.
- `theme` — palette / colour mapping.
- `error` — `EngineError`.

The crate also re-exports the public PTY types so the application
depends on this crate as the single engine entry point:
`Dims`, `EventProxy`, `ScreenTerm`, `SharedDims`, `SharedMaster`,
`SharedWriter`, `apply_resize`.

## Public API surface

```rust
use orkia_terminal_core::{
    AdoptMaster, EngineConfig, TerminalEngine, wake_pair,
};
use orkia_pty::open_pair;

let adopted = open_pair(120, 40)?;
let (wake_tx, wake_rx) = wake_pair();

let engine = TerminalEngine::adopt_master(AdoptMaster {
    reader: adopted.reader,
    writer: adopted.writer,
    master_fd: adopted.master_fd,
    dims: adopted.dims,
    screen: adopted.screen,
    buf_bytes: 64 * 1024,
    on_osc133: None,
    on_apc: None,
})?;

// In the UI loop, await `wake_rx` and pull a snapshot from the engine.
```

For spawned children (agent jobs), `TerminalEngine::start(EngineConfig)`
opens a PTY internally via `orkia-pty::spawn_config`.

## Consumed by

- The application binary (`bins/orkia`) and the TUI renderer
  (`orkia-shell-tui`), which feed terminal snapshots into the
  attached-PTY widget.
- The job controller in `orkia-shell` when it spawns agent jobs.

## Development notes

- Lock-free where it counts: the snapshot publish step uses an
  `Arc` swap, the wake channel is single-consumer, the grid is
  owned by exactly one thread.
- The reader thread reads in 64 KiB chunks by default
  (`buf_bytes`). Smaller buffers measurably regress throughput on
  high-volume agent output; benchmark before changing.
- The extractor exists so the render side never touches the grid
  mutex; the snapshot is the only thing rendering code sees.

## License

`Elastic-2.0`; see [`../../LICENSE`](../../LICENSE).

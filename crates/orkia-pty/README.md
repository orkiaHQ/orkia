# orkia-pty

PTY abstraction over `portable-pty` + `alacritty_terminal`. Opens a
pseudo-terminal, optionally spawns an interactive child behind it,
and exposes the raw read/write/resize handles plus a fixed-grid
display `Term` for snapshotting.

## Overview

This crate is the lowest layer of the Orkia terminal stack. It owns
nothing long-lived — every entry point returns a struct of typed
handles (`PtyProcess` for spawned children, `AdoptedPty` for the
embed case) and lets the caller decide what to do with them. There
is no reader thread here, no block parsing, no snapshotting, and no
user-facing strings: just typed `PtyError`s and `tracing`
diagnostics.

Two entry points are exported:

- `spawn_config(SpawnConfig)` opens a PTY pair and spawns a child
  command in it. Used by `TerminalEngine::start` to launch agent
  jobs (`claude`, `codex`, `gemini`, …) inside their own PTY.
- `open_pair(cols, rows)` opens a raw PTY pair via `openpty(3)` and
  returns both fds (`master_fd`, `slave`) without spawning anyone.
  Used to embed a Rust-native shell engine (the forked `brush`) in-
  process by handing it the slave fd through its `open_files`
  hooks.

The display-side `Term` is created with `scrolling_history = 0`:
it's a fixed grid the engine renders into for snapshots. Real
scrollback lives one layer up.

The reader thread that pulls bytes off the PTY and drives the
alacritty grid is intentionally not in this crate — it lives in
`orkia-terminal-core` as the validated three-thread model.

## Key types

- `PtyError` — `Io(std::io::Error)` and `Backend(String)` (a
  stable typed wrapper around `portable-pty`'s `anyhow::Error`).
- `Dims` — fixed-grid geometry helper implementing
  `alacritty_terminal::grid::Dimensions`.
- `EventProxy` — alacritty `EventListener` that forwards `PtyWrite`
  events (DSR/DA query responses) into the PTY writer so embedded
  TUIs don't hang waiting for replies.
- `SharedWriter`, `SharedMaster`, `SharedDims`, `ScreenTerm` —
  shared handle type aliases (`Arc<Mutex<…>>` / `Arc<FairMutex<…>>`).
- `SpawnConfig` — builder-style struct for `spawn_config`.
  `SpawnConfig::command(cmd, args, cols, rows)` is the common
  helper.
- `PtyProcess` — handles for a spawned child: `writer`, `reader`,
  `master`, `dims`, `screen`, `child`. Has `child_id()`,
  `send_signal(sig)` (Unix), and `try_wait()`.
- `AdoptedPty` — handles for an embedded engine: `reader`,
  `writer`, `master_fd` (kept alive for ioctls), `slave`, `dims`,
  `screen`.

## Public API surface

Spawn an agent in its own PTY:

```rust
use orkia_pty::{spawn_config, SpawnConfig};

let pty = spawn_config(SpawnConfig::command(
    "claude",
    vec!["chat".into()],
    100, 30,
))?;
let _pid = pty.child_id();
```

Open a raw pair and hand the slave to an embedded shell engine:

```rust
use orkia_pty::open_pair;

let adopted = open_pair(120, 40)?;
// adopted.slave  → hand to brush via open_files
// adopted.reader → drive the terminal engine
// adopted.writer → inject OSC-133 sequences around commands
```

Resize on a pane change:

```rust
use orkia_pty::{apply_resize, resize_adopted};

apply_resize(&pty.master, &pty.screen, &pty.dims, 120, 40);
resize_adopted(&adopted.master_fd, &adopted.screen, &adopted.dims, 120, 40)?;
```

## Consumed by

- `orkia-terminal-core` — owns the reader thread driving these
  handles and re-exports the public types so the application
  binds to a single engine entry point.
- The shell's job controller, which spawns agent jobs.

## Development notes

- Sets `FD_CLOEXEC` on the master fd in `open_pair` so it doesn't
  leak into brush's grandchildren. The slave stays inheritable on
  purpose so brush's children inherit it.
- `TIOCSWINSZ` is used directly for adopted-PTY resize.
- The crate uses `unsafe` for `openpty(3)`, `fcntl`, `ioctl`,
  `kill`, and `OwnedFd` construction. Each block is annotated
  with a SAFETY comment documenting the precondition.

## License

`Elastic-2.0`; see [`../../LICENSE`](../../LICENSE).

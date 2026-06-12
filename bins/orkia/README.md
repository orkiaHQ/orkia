# orkia (binary)

The `orkia` CLI binary — the entry point users run, set as their
login shell, or invoke from `ssh` / `cron` / scripts.

## Overview

`orkia` has three runtime shapes:

- `orkia -c "<cmd>"` — non-interactive single-shot. Boots the
  in-process brush engine, sources `~/.orkiarc`, runs the
  command, exits with its status code. This is the safety net
  for `chsh`, `ssh user@host 'cmd'`, and cron.

  **`-c` is POSIX-first.** The payload goes to brush exactly as
  a `bash -c` payload would: `orkia -c "ps"` runs the system
  `ps`, never the Orkia builtin. Only two shapes route through
  the REPL pipeline instead: an explicit `orkia ` namespace
  (`orkia -c "orkia ps"` → the builtin) and agent commands
  (`@agent …`, `cmd | @agent`). Scripts get byte-for-byte POSIX
  behavior unless they opt into Orkia by name.
- `orkia --tui` — launches directly into the ratatui alternate
  screen (sidebar, scrollable blocks, attached-PTY widget). Same
  surface available from inside shell mode via the `tui` builtin.
- `orkia` (default, interactive) — stdin/stdout shell mode, like
  zsh: native terminal scrollback and selection are preserved.
  If stdin or stdout is not a TTY (piped input, CI), the binary
  falls through to the minimal `StdoutRenderer` instead.

In addition, four CLI subcommands short-circuit the REPL:

- `orkia setup [--minimal] [--force] [--offline]` — first-time
  setup wizard. Scaffolds `~/.orkia/`, installs default agents,
  and is re-runnable to add more.
- `orkia migrate-rc [--from PATH] [--dry-run] [--append]` —
  converts an existing `~/.zshrc` / `~/.bashrc` / fish config
  into `~/.orkiarc`. Composes with pipes when `--dry-run` is
  set.
- `orkia bridge --source <claude|codex|gemini|generic>` —
  agent-hook shim. Reads a JSON payload on stdin and forwards it
  to the journal socket at `~/.orkia/run/orkia.sock`. Used by
  agent hook configs; not intended for direct human use.
- `orkia journal [filters]` — query
  `~/.orkia/journal.jsonl`. Works without a running REPL.

## Boot sequence

`main()` (in `src/main.rs`) does the following, in order:

1. `init_tracing()` — installs an `EnvFilter` from `RUST_LOG`
   (default `warn`). If `ORKIA_LOG=<path>` is set, tracing is
   routed to that file (no ANSI) so an interactive shell stays
   clean and a `tail -f` session can watch it from another tab.
2. `install_sigint_swallow()` — installs handlers for `SIGINT`,
   `SIGQUIT`, and `SIGTSTP`. When a job is attached
   (`orkia_shell::job::foreground::attached_pid()`), `SIGINT` is
   forwarded to the live descendant of the attached PID
   (agents like claude fork a successor and let the wrapper
   exit, so the original PID returns ESRCH); `SIGQUIT` / `SIGTSTP`
   request a clean detach. When no job is attached, all three are
   swallowed so a stray Ctrl-C / Ctrl-\ / Ctrl-Z does not kill or
   suspend the shell.
3. `parse_args()` — hand-rolled flag parser (no clap). Detects
   login-shell invocation from a leading `-` in `argv[0]`
   (`chsh -s` convention) and short-circuits subcommands.
4. Subcommand dispatch — `setup`, `migrate-rc`, `bridge`, and
   `journal` are routed before any config load or rc sourcing
   so they work on an unconfigured box.
5. `ShellConfig::load()` — read `~/.orkia/config.toml`.
6. If `-c <cmd>` was passed, `run_dash_c` constructs a
   `ShellEngine`, sources `.bashrc` / `.profile` / `.orkiarc`
   per config, executes the command, and exits with its code.
7. `init::auto_prompt_if_missing()` — on first launch in an
   interactive terminal, prompt the user to run `orkia setup`.
   Non-fatal: the user can run it later.
8. Renderer selection. Decision tree:
   1. `--tui` on the command line → `TuiRenderer`.
   2. `--no-tui` on the command line → `StdoutRenderer`.
   3. `config.default_mode == "tui"` → `TuiRenderer`.
   4. stdin or stdout is not a TTY → `StdoutRenderer`.
   5. otherwise → `ShellModeRenderer`.
9. Construct `Repl::new(renderer, HeuristicClassifier,
   HeuristicRouter, config)`, install the `TuiFactory` closure
   (so the `tui` builtin can swap renderers at runtime without
   `orkia-shell` taking a dep on `orkia-shell-tui`), and call
   `repl.run().await`.

If `TuiRenderer::new` fails (no TTY, raw-mode error, …), the
binary falls back to `ShellModeRenderer` and reports the error
to stderr — losing the TUI is never fatal.

## Source layout

- `src/main.rs` — `main`, argument parsing, signal handlers,
  renderer selection, subcommand dispatch.
- `src/setup/` — `orkia setup` wizard logic and the
  `auto_prompt_if_missing` first-launch hook.
- `src/bridge.rs` — `orkia bridge` shim: parses arguments,
  reads stdin payload, posts to the journal socket.
- `src/journal_cli.rs` — `orkia journal` query handler.

## Environment

- `RUST_LOG=<filter>` — tracing filter (e.g. `info`,
  `orkia_shell=trace,brush_core=warn`). Default `warn`.
- `ORKIA_LOG=<path>` — route tracing output to a file instead of
  stderr. Strongly preferred for live debugging an interactive
  session.

## Build / install

```bash
cargo build --release --bin orkia
# Install as a login shell candidate:
sudo cp target/release/orkia /usr/local/bin/orkia
echo /usr/local/bin/orkia | sudo tee -a /etc/shells
chsh -s /usr/local/bin/orkia
```

## License

`Elastic-2.0`; see [`../../LICENSE`](../../LICENSE).

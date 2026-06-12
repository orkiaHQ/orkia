# orkia-builtin

Builtin commands for the Orkia shell. Each builtin is a small,
side-effect-light module that returns a `Vec<BlockContent>` — pre-
rendered blocks the active renderer turns into terminal output.

## Overview

Builtins are the shell's first-class verbs: things like `ps`,
`aroute`, `approve`, `agent`, `briefing`, `rfc`, `issue`, `project`,
`config`, `history`, `kill`, `migrate-rc`. They live in their own
crate so the REPL (`orkia-shell`) can dispatch them via a thin
match on `BuiltinCmd` and so they can be unit-tested without
spinning up the full REPL.

This crate intentionally has a minimal dependency footprint —
just `orkia-shell-types` for the shared `BlockContent` /
`AgentInfo` / `JobInfo` / `PsFlags` shapes, plus `serde_json` for
the `--json` flag handlers. The SEAL builtin lives in
`orkia-shell::seal::builtin` because it needs SHA-256 and the
chain types, which would bloat this crate's dependency list.

The output convention is uniform: each public entry point returns
`BuiltinResult = Vec<BlockContent>`. The REPL feeds those blocks to
the active renderer, which decides how to draw them in shell mode
vs the ratatui TUI.

## Modules

- `ps` — `ps(agents, jobs, flags)`: render the agent + jobs table
  (and an optional system-processes section when `--system` is
  passed). Supports `--json` for scripting.
- `aroute` — `aroute list|set|test`: inspect and override the
  router's mapping of intents to agents.
- `approve` / (paired with the REPL's `ApprovalWatcher`) — accept
  or deny a pending agent action; updates the on-disk queue.
- `agent` — `agent list|show|add|remove|enable|disable`: manage
  agent definitions on disk.
- `agent_templates` — built-in agent templates installed by
  `orkia setup`.
- `briefing` — render the daily briefing block (open issues,
  recent agent activity, pending approvals).
- `rfc` — `rfc list|show|new`: list, view, scaffold RFC markdown
  files in the workspace.
- `issue` — `issue list|show|new|close`: lightweight local issue
  tracker that lives under the workspace directory.
- `project` — `project list|show|switch|init`: manage project
  membership of the current workspace.
- `kill` — render the `kill` builtin's output (the actual signal
  delivery is performed by the REPL via `JobController`).
- `history` — render shell history with filters.
- `config` — print or edit shell configuration entries.
- `help` — render the builtin help table.
- `migrate_rc` — `MigrateRcOpts`, `run_migration`: convert
  `~/.zshrc` / `~/.bashrc` / fish config into `~/.orkiarc`. Used
  both from the REPL builtin and from `orkia migrate-rc` at the
  CLI level.

## Public API surface

```rust
use orkia_builtin::{ps, BuiltinResult};
use orkia_shell_types::{AgentInfo, JobInfo, PsFlags};

fn render_ps(
    agents: &[AgentInfo],
    jobs: &[JobInfo],
    flags: &PsFlags,
) -> BuiltinResult {
    ps::ps(agents, jobs, flags)
}
```

The migration entry point is shaped for both the REPL builtin and
the CLI:

```rust
use orkia_builtin::migrate_rc::{MigrateRcOpts, run_migration};

let opts = MigrateRcOpts::parse(&["--dry-run".into()])?;
let report = run_migration(&opts, &home, &dest, "2026-05-21")?;
```

## Consumed by

- `orkia-shell` — dispatches `BuiltinCmd` variants to these entry
  points from the REPL.
- `bins/orkia` — calls `migrate_rc::run_migration` directly when
  invoked as `orkia migrate-rc ...`.

## Development notes

- Each builtin module is self-contained and side-effect-light;
  filesystem I/O is performed via paths passed in by the caller,
  not via global state.
- `uuid` is a dev-dependency for tests that need to fabricate
  workspace IDs.

## License

`Elastic-2.0`; see [`../../LICENSE`](../../LICENSE).

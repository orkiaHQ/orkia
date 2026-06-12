+++
# Self-check fixture for script/rfc-lint.
#
# This file is the worked example shown in docs/rfc-primitive.md. It must
# pass `script/rfc-lint script/fixtures/prompt-history-search.md` cleanly.
# If the linter rejects it, either the linter or the doc has drifted —
# fix the drift before changing this fixture.
id          = "prompt-history-search"
title       = "Searchable Prompt History"
state       = "draft"
authors     = ["@contributor"]
created_at  = "2026-05-24"
tags        = ["shell", "ux"]
priority    = "medium"
+++

## Context

Today Orkia's REPL has line-edit history (up-arrow) but no incremental
search. Users coming from bash/zsh expect `Ctrl-R` to scan back through
prior prompts and surface matches as they type. The absence is the most
frequent UX complaint on the issue tracker.

## Goals

- A user can press `Ctrl-R` and type to find a previous prompt by substring.
- Matches stream in as the query is typed; no perceptible REPL hitch.
- The feature works equally for shell passthrough lines and `@agent` dispatches.

## Constraints

- **REPL main loop is sacred** (CLAUDE.md): the history search must not
  block the REPL while scanning. The scan runs in a dedicated thread and
  streams matches back through the existing event channel; the REPL only
  renders snapshots on its normal drain tick.
- **One owner per resource** (CLAUDE.md): the on-disk history file remains
  owned by the journal thread. The search thread reads via a request /
  response channel rather than opening the file directly.
- No PTY interaction in this RFC — *treat every byte as untrusted* does
  not apply.

## Approach

Add a `HistorySearch` mode to the REPL state machine, entered on `Ctrl-R`
and exited on `Enter` / `Esc`. The render path lives in
`crates/orkia-shell-tui/src/widgets/history_search.rs` (new). The scan
runs in a thread spawned from `orkia-shell/src/history/search.rs` (new)
and communicates via a `HistorySearchEvent` channel added to the existing
shell event enum at `orkia-shell-types/src/events.rs:NN`.

## Tasks

- [ ] Add `HistorySearchEvent` variant to the shell event enum.
- [ ] Implement the scan thread with a request/response channel against the journal owner.
- [ ] Add the `HistorySearch` REPL state and key bindings.
- [ ] Add the TUI widget and wire it to the renderer.
- [ ] Integration test against a real `orkia` session with seeded history.

## Acceptance Criteria

- `cargo clippy -- -D warnings` passes on the affected crates.
- Running `orkia` in a real terminal, pressing `Ctrl-R` and typing a
  substring of a prior prompt surfaces matches without dropped keystrokes.
- The REPL draws a new prompt within 16ms of `Esc` exiting search mode,
  measured by the existing render-latency harness.

## Alternatives Considered

- **Reuse bash's `Ctrl-R` via passthrough.** Rejected: only works when the
  current line would route to `$SHELL`; agent dispatches and Orkia builtins
  would have no history search, which is exactly the gap users complain about.
- **Synchronous in-REPL scan over an in-memory ring buffer.** Rejected:
  trivial to implement but caps history at the ring size and violates the
  "REPL main loop is sacred" principle the moment the buffer grows past a
  few thousand entries.

## Open Questions

- The scan currently re-reads the full history file on every query.
  Acceptable up to ~10k entries; beyond that we need an index. Should the
  index land in this RFC or as a follow-up once users hit the limit?
- `Ctrl-R` is the obvious binding but currently belongs to bash passthrough.
  Override unconditionally, or only when the cursor is at the start of an
  empty line?

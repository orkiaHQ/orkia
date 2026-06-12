# orkia-shell-tui

`ratatui` + `crossterm` implementation of `ShellRenderer` — the
alternate-screen TUI for the Orkia shell.

## Overview

`TuiRenderer` is the renderer that runs when `orkia --tui` is used
(or when `default_mode = "tui"` is set in
`~/.orkia/config.toml`). It enters the alternate screen, switches
the terminal into raw mode, and lays out a three-region UI: a
sidebar listing agents, jobs and workspace items; a scrollable
main pane that accumulates `BlockContent` blocks emitted by the
REPL; and an input + status bar at the bottom.

Like the shell-mode renderer, `TuiRenderer` implements the
`ShellRenderer` trait from `orkia-shell-types`. The REPL drives it
with `publish(RenderEvent)` and pulls the next user line via
`read_line(&PromptContext)`. The renderer is also responsible for
the "attached" sub-mode, where the user has attached to a running
agent PTY: keystrokes are translated to PTY bytes, the PTY
output is rendered into the main pane, and the renderer watches
for a detach gesture (Ctrl-Z, configurable) to hand control back
to the REPL.

The renderer is constructed at startup with an owned snapshot of
the agent list and the workspace; subsequent updates arrive as
`JobsSnapshot` / `WorkspaceSnapshot` render events so there is no
shared state between the REPL and the renderer.

## Output Source Navigation

The `Output` view can extract projection source references from the
selected output card. When the detail pane is open, `j` / `k` move
between source references, and `o` resolves the selected reference
through the shell-backed `operator open` path. The resolved content is
shown in the same detail surface used by command-backed inspections.

Supported references include `kg://...`, `kg:<prefix>`,
`journal://event/<n>`, `journal:<n>`, and `seal:<n>`.

## Modules

- `renderer` — `TuiRenderer`. Holds the ratatui `Terminal`, the
  layout, theme, accumulated blocks, scroll offset, input buffer,
  current agent / job / workspace snapshots, pending-approvals
  counter, current working directory, and the optional
  `AttachedJob` state.
- `layout` — `ShellLayout`, `LayoutRects`. Splits the available
  `Rect` into sidebar / main / input regions and the attached
  status overlay.
- `theme` — `Theme`: colour palette, role colours, accents.
- `widgets` — `PtyWidget` plus the dedicated renderers
  (`render_sidebar`, `render_main_pane`, `render_input_bar`,
  `render_status_bar`, `render_briefing`, `render_attached_footer`,
  `render_attached_status`).
- `attached` — `AttachedJob`, `AttachedAction`, `DriveExit`,
  `classify_attached_key`, `key_to_pty_bytes`. The keystroke and
  polling state machine for the attached sub-mode.

## Public API surface

```rust
use orkia_shell_tui::TuiRenderer;
use orkia_shell_types::{AgentInfo, Workspace};

let renderer = TuiRenderer::new(agents, workspace)?;
// hand the renderer to `Repl::new(renderer, classifier, router, config)`.
```

`TuiRenderer::new` enters the alternate screen and raw mode and
installs a panic hook that restores the terminal — this is
important because the shell is the user's primary terminal: a
panic must not leave them in raw mode.

## Consumed by

- `bins/orkia` — constructs `TuiRenderer` when `--tui` /
  `default_mode = "tui"` is selected, and also installs a
  `TuiFactory` closure on the REPL so the in-shell `tui` builtin
  can swap renderers at runtime.

## Development notes

- `deny(warnings)`, `deny(clippy::unwrap_used)`,
  `deny(clippy::expect_used)`.
- Dependencies are deliberately thin: `ratatui`, `crossterm`,
  `unicode-width` for grapheme-aware truncation, and the shared
  `orkia-shell-types` / `orkia-terminal-core` crates.
- The attached sub-mode polls at `ATTACHED_POLL_MS` and converts
  crossterm `KeyEvent`s to PTY byte sequences via
  `key_to_pty_bytes`. Adding a new key binding means editing the
  `classify_attached_key` table.

## License

`Elastic-2.0`.

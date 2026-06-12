# orkia-shell-types

Shared types and trait definitions for the Orkia shell. This crate holds
the vocabulary that the REPL, classifier, router, builtins, renderers,
and CLI binary all agree on; it deliberately contains no I/O and no
business logic.

## Overview

The shell core (`orkia-shell`) is split into many subsystems: a REPL
loop, a classifier, an agent router, a job controller, a SEAL chain
emitter, an approval watcher, a journal store, multiple renderers. Each
of those subsystems is talked to via plain types and traits. Putting
those types in a leaf crate keeps `orkia-shell` from being a hub of
circular dependencies and lets other workspace crates
(`orkia-builtin`, `orkia-shell-tui`, the CLI binary) consume the same
shapes without pulling in the full shell engine.

The crate exposes data types (`Decision`, `Mode`,
`JobInfo`, `AgentInfo`, `SealRecord`, `Workspace`, …), enums for
shell-side state (`AgentStatus`, `JobState`,
`JobKind`, …), and a small number of public traits
(`IntentClassifier`, `AgentRouter`, `ShellRenderer`) that the REPL
takes as generic parameters.

## Key modules

- `agent` / `agent_def` — `AgentInfo`, `AgentStatus`,
  and the on-disk `AgentDefinition` / `AgentConfigFile` /
  `AgentToolsFile` structures (TOML schemas for agent definitions,
  context sections, runtime config, tool registries, MCP servers).
- `decision` — the central `Decision` enum (`Shell`, `Builtin { name,
  args }`, `Exec`, `Agent`, `Pipeline`, `NoOp`); builtins are resolved
  by `name` through the `exec` command registry (`trait Command`,
  dispatched in `Repl::dispatch_named`), not a `BuiltinCmd` enum (that
  enum was removed in SPEC-ORKIA-EXEC-MIGRATION-V1 Vague 5). Also
  `Mode`, `BlockContent`, `Outcome`, `PipelineStage`,
  `ApprovalStatus`, `NoOpReason`.
- `classifier` — the `IntentClassifier` trait and `IntentGuess`.
- `router` — `AgentRouter` trait, `RoutingDecision`, `RoutingReason`.
- `renderer` — `ShellRenderer` trait, `RenderEvent`, `PromptContext`,
  `WelcomeInfo`. Renderers receive `RenderEvent`s from the REPL and
  return user input via `read_line`.
- `job` — `JobId`, `JobInfo`, `JobKind`, `JobState`, `JobEvent` —
  shared job lifecycle types between the controller in `orkia-shell`
  and the renderers / builtins.
- `seal` — `SealRecord` (canonical chained shell-event record).
- `workspace` — `Workspace`, `Project`, `IssueSummary`, `RfcSummary`,
  `RfcFrontmatter`, plus `parse_rfc_frontmatter`.
- `attached` — `AttachedHandle`, `AttachedOutcome`, `LivenessProbe`
  describing the renderer-side contract for attaching to an agent PTY.
- `builtin_flags` — flag structs (e.g. `PsFlags`) consumed by the
  builtins.
- `history` — `HistoryEntry`, `HistoryType` for the persistent shell
  history.
- `error` — `ShellError`.

## Public API surface

A renderer is anything that implements `ShellRenderer`:

```rust
use orkia_shell_types::{
    PromptContext, RenderEvent, ShellRenderer,
};

struct MyRenderer;

impl ShellRenderer for MyRenderer {
    fn publish(&mut self, _event: RenderEvent) {}
    fn read_line(&mut self, _ctx: &PromptContext) -> Option<String> {
        Some("ls".into())
    }
}
```

An agent router is a function from `(intent, &[AgentInfo])` to an
optional `RoutingDecision`:

```rust
use orkia_shell_types::{AgentInfo, AgentRouter, RoutingDecision, RoutingReason};

struct OnlyOne;

impl AgentRouter for OnlyOne {
    fn route(&self, _intent: &str, agents: &[AgentInfo]) -> Option<RoutingDecision> {
        let only = agents.first()?;
        Some(RoutingDecision {
            agent_name: only.name.clone(),
            confidence: 1.0,
            reason: RoutingReason::OnlyOption,
        })
    }
}
```

The REPL in `orkia-shell` is generic over both traits, so test code can
plug in deterministic implementations without spinning up the real
heuristics.

## Consumed by

- `orkia-shell` — the REPL takes `C: IntentClassifier` and
  `A: AgentRouter` as type parameters and a `Box<dyn ShellRenderer>`.
- `orkia-builtin` — builtins return `Vec<BlockContent>` and consume
  the flag structs from this crate.
- `orkia-shell-tui` — `TuiRenderer` implements `ShellRenderer`.
- `bins/orkia` — picks a renderer at startup based on flags / TTY
  detection.

## Development notes

- No `unsafe`. Serde is used for the agent-definition TOML schemas and
  the SEAL record.
- `tempfile` is the only dev-dependency; tests live next to the
  modules they cover.

## License

`Elastic-2.0`; see [`../../LICENSE`](../../LICENSE).

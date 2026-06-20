# Changelog

All notable changes to Orkia are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project adheres to [Semantic Versioning](https://semver.org/).

Versions before `1.0.0` are pre-release; backwards-incompatible
changes may land in minor bumps.

## [Unreleased] — next (incoming)

### Added

- **Convergence loop for RFC dispatch.** Dispatch no longer stops at "the
  agents ran" — it drives a run to *verifiably done* against acceptance
  oracles, bounded and sealed.
  - **Per-task acceptance + self-repair (V1).** A task in an RFC's
    `[dispatch]` block may declare `accept = "<shell command>"` and
    `max_attempts`. After the agent's turn, the proxy runs the oracle
    off-actor (`/bin/sh -lc`); on failure it re-injects the task for a
    bounded number of attempts before rejecting. The kernel still sees one
    outcome per task — the loop is entirely proxy-local.
  - **Per-RFC fleet convergence (V2).** The `[dispatch]` block accepts a
    top-level `accept` (integration oracle) and `max_replans`. Once the task
    DAG drains, the proxy runs the integration oracle; on failure it drives a
    bounded re-plan loop until the RFC converges, the re-plan budget is
    exhausted (`integration-failed`), or the same failure repeats
    (`oscillating`, detected by a failure-signature hash).
  - **Targeted re-plan (premium brain).** When a kernel brain is wired, the
    proxy hands it the integration-failure tail and the brain re-opens only
    the implicated tasks plus their transitive dependents, keeping the rest
    of the DAG `Done`. Without a brain, the proxy falls back to re-running the
    whole DAG. A local model can attribute the failure to specific tasks; on a
    miss it falls back to a leaf heuristic.
- **Convergence provenance in SEAL.** New decision kinds —
  `AcceptanceVerdict`, `GlobalVerdict`, and `ReplanDecision` — are recorded in
  the per-RFC hash-chained `dispatch.seal.jsonl`, so every oracle verdict and
  re-plan decision is reconstructable and verifiable after the fact.

## [0.1.0] — Initial public release

First source-available release of the orkia shell.

### Added

- **Agentic shell core.** A `brush`-backed POSIX shell that hosts AI agents
  (Claude Code, Codex, Gemini) as governed Unix jobs inside isolated PTYs:
  `@agent <prompt>` dispatch, `ps`, `attach`, `tell`, `kill`, and job control.
- **SEAL audit chain.** A signed, tamper-evident JSONL chain per agent job and
  project; replay and verify with the `audit` builtin (`audit --job <id>`,
  `audit verify`).
- **RFC workflow.** The RFC primitive as a first-class concept — `rfc` builtin,
  `rfc seal <slug>` for signed RFC documents, and the contributor RFC flow
  (`rfcs/TEMPLATE.md`, `script/rfc-lint`).
- **Cage execution boundary** (`orkia-cage`, `orkia-sh`) mediating per-command
  agent actions, with capability resolution via the `cap` builtin.
- **TUI and stdout renderers**, single-shot `-c <command>` mode, and CLI
  subcommands (`setup`, `migrate-rc`, `bridge`, `journal`).
- **Plugin SDK** (`orkia-plugin-sdk`, Apache-2.0) and sandboxed plugin host.
- Signed-binary installer at `https://orkia.dev/install`.

### Notes

- Source-available under the Elastic License 2.0; plugin SDK and build tooling
  under Apache-2.0. See [`LICENSE`](LICENSE).
- Reasoning-graph and Forge crates ship as preview / QA-scoped surfaces (see
  the Status section in the README).

# Changelog

All notable changes to Orkia are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project adheres to [Semantic Versioning](https://semver.org/).

Versions before `1.0.0` are pre-release; backwards-incompatible
changes may land in minor bumps.

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

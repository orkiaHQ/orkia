# orkia-entities-core

Canonical entity types shared across the Orkia ecosystem.

## Overview

`orkia-entities-core` defines the serde-derived structs that
describe the durable entities Orkia operates on: agents, workspaces,
projects, teams, issues, RFCs and their messages and versions,
approvals, archetypes, SEAL records, shared session excerpts, and
the wire envelopes used for bootstrap and delta sync.

The crate is intentionally a leaf with no business logic. It holds
the data shapes that move between the shell, the sync layer, any
server-side component, and persistence — keeping them in one place
guarantees that every consumer agrees on field names, types, and
optionality. The crate enforces `deny(warnings)`,
`deny(clippy::unwrap_used)`, and `deny(clippy::expect_used)`.

## Modules

- `account` — `AccountCore`.
- `agent` — `AgentCore`: id, workspace, name, archetype, avatar
  seed, trust score, trust dimensions, memory blob, config blob,
  status, runtime mode, governance policy, custom instructions,
  scope, step / temperature limits, LLM binding, timestamps.
- `archetype` — `ArchetypeCore`.
- `approval` — `ApprovalCore`.
- `rfc`, `rfc_message`, `rfc_message_mention`, `rfc_version` —
  the RFC document model and its message log.
- `issue`, `issue_branch`, `issue_comment`, `issue_event`,
  `issue_share` — the issue tracker model.
- `project`, `project_clone`, `project_member` — project model.
- `team`, `team_member`, `workspace`, `workspace_invite` —
  workspace / team membership.
- `seal_record` — `SealRecordCore`, the durable form of a shell
  SEAL chain entry.
- `shared_session_excerpt` — `SharedSessionExcerptCore`.
- `cli_raw_event` — `CliRawEventCore`, the un-redacted form of a
  raw CLI event captured for replay/debug.
- `rejection` — `MutationRejectionCode`, the canonical reason
  enum used when a mutation is denied by policy.
- `enums` — shared enums (`AgentRuntimeMode`, `AgentStatus`, …).
- `wire` — bootstrap and delta envelopes (`BootstrapMeta`,
  `EntityLine`, `DeltaLine`, `DeltaEnd`, `BootstrapFailureKind`).

## Public API surface

Every entity has a single public struct, suffixed `Core`, and a
re-export at the crate root. A representative shape:

```rust
use orkia_entities_core::AgentCore;
use chrono::{DateTime, FixedOffset};
use uuid::Uuid;

fn fingerprint(agent: &AgentCore) -> (Uuid, &str, &DateTime<FixedOffset>) {
    (agent.id, agent.name.as_str(), &agent.updated_at)
}
```

Wire envelopes are read line-by-line for bootstrap streams:

```rust
use orkia_entities_core::wire::{EntityLine, DeltaLine, DeltaEnd};
```

## Consumed by

- The shell core, sync layer, and any external persistence /
  server component. Anywhere two Orkia processes need to agree on
  the shape of an entity, they should depend on this crate.

## Development notes

- No `unsafe`, no I/O. The crate exposes plain data plus serde
  derives.
- Each `*_Core` struct lives in a private module and is re-exported
  from `lib.rs` to keep the public surface flat.
- License headers use SPDX `Elastic-2.0` at the file level, matching the
  crate package license metadata.

## License

`Elastic-2.0`; see [`../../LICENSE`](../../LICENSE).

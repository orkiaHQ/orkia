# RFCs in Orkia

An RFC is a structured intention — a markdown document that describes what should be done, why, and with what constraints, written before the code so reviewers can engage with the design rather than the implementation. Orkia uses RFCs (and not GitHub issues or PR descriptions) because the same primitive runs the product: an RFC you contribute to this repository has the same shape as one you create inside Orkia at runtime, so the contribution flow and the product surface stay unified. An RFC is not a ticket, not a PR description, and not a chat log.

## When to write one

| Use an RFC for                                          | Skip the RFC for             |
|---------------------------------------------------------|------------------------------|
| New features and public surface changes                 | Bug fixes and typos          |
| Behavior changes users will notice                      | Dependency bumps             |
| Architectural changes, shell core, or the agent contract | Small internal refactors     |

The threshold question: *would a reviewer ask "why?" before accepting this change?* If yes, write an RFC. If no, open a PR.

## Lifecycle

Four states, mapped to the GitHub workflow:

```
draft  →  active  →  completed
            ↓
        abandoned
```

- `draft → active`: a maintainer merges the RFC PR. Discussion happens on the PR; the merge is the approval.
- `active → completed`: implementation PRs land and the RFC file is moved to `rfcs/completed/`.
- `active → abandoned`: the RFC is closed without implementation and moved to `rfcs/abandoned/`, with a reason recorded in the move commit.

That is the full lifecycle. State is recorded in the `state` frontmatter field and reflected by the file's directory.

## File format

Every RFC lives at `rfcs/<slug>.md`, opens with TOML frontmatter, and follows a fixed body structure.

```markdown
+++
id          = "prompt-history-search"
title       = "Searchable Prompt History"
state       = "draft"
authors     = ["@contributor"]
created_at  = "2026-05-24"
tags        = ["shell", "ux"]
priority    = "medium"
issue       = "GH123"          # omit this field if there is no linked issue
+++
```

| Field        | Type          | Required | Notes                                                |
|--------------|---------------|----------|------------------------------------------------------|
| `id`         | string (slug) | yes      | kebab-case; must match the filename minus `.md`      |
| `title`      | string        | yes      | human-readable headline                              |
| `state`      | enum          | yes      | `draft` \| `active` \| `completed` \| `abandoned`    |
| `authors`    | string array  | yes      | GitHub handles prefixed with `@`                     |
| `created_at` | ISO date      | yes      | `YYYY-MM-DD`                                         |
| `tags`       | string array  | no       | free-form                                            |
| `priority`   | enum          | no       | `low` \| `medium` \| `high` \| `critical`            |
| `issue`      | string        | no       | linked GitHub issue, e.g. `GH123`                    |

The delimiter is `+++`, not `---`. See `rfcs/TEMPLATE.md` for a working example.

## Required body sections

All eight sections must be present, in this order:

- **Context** — the situation that motivates the change.
- **Goals** — what success looks like, in user-visible outcomes.
- **Constraints** — what the design must respect, including the relevant principles from `CLAUDE.md`.
- **Approach** — the proposed design, with `path/to/file.rs:42` references to existing code.
- **Tasks** — the work items, as a markdown checklist.
- **Acceptance Criteria** — testable invariants that define done.
- **Alternatives Considered** — at least two genuine alternatives with reasons for rejection.
- **Open Questions** — what remains unresolved; must be non-empty.

Sections must be present even when they feel light — for a small change, a one-line "N/A: this RFC does not introduce new state, no constraint from this principle applies" is acceptable, but the heading stays.

The last section is load-bearing. *If you cannot think of an open question, you have not thought about the design deeply enough.* Reviewers will surface them anyway; better to do it yourself.

### Constraints, done well

The two sections contributors most often fumble are *Constraints* and *Open Questions*. Examples beat explanation.

A well-formed *Constraints* entry names a principle from `CLAUDE.md` by its heading and explains how the design respects it:

```markdown
- **REPL main loop is sacred** (CLAUDE.md): the history search must not
  block the REPL while scanning. The scan runs in a dedicated thread
  and streams matches back through the existing event channel; the
  REPL only renders snapshots on its normal drain tick.
- **One owner per resource** (CLAUDE.md): the on-disk history file
  remains owned by the journal thread. The search thread reads via a
  request/response channel rather than opening the file directly.
```

If a principle does not apply, say so explicitly (`"No PTY interaction in this RFC — *treat every byte as untrusted* does not apply."`). A missing principle reads as oversight; a noted-and-dismissed principle reads as care.

### Open Questions, done well

Bad — formalities that signal "I didn't think":

```markdown
- Are there edge cases?
- Is the API name good?
- Should we add tests?
```

Good — genuine forks where reviewer input changes the design:

```markdown
- The scan currently re-reads the full history file on every query.
  Acceptable up to ~10k entries; beyond that we need an index. Should
  the index land in this RFC or as a follow-up once users hit the limit?
- `Ctrl-R` is the obvious binding but currently belongs to bash
  passthrough. Override unconditionally, or only when the cursor is at
  the start of an empty line?
```

The test: a good open question is one where two reasonable reviewers could disagree, and the disagreement would change what you build.

## How to write one

1. Find or open a GitHub issue describing the problem. Wait for a maintainer to label it `ready-to-rfc` — this is the signal that the problem is worth the design cost.
2. Copy `rfcs/TEMPLATE.md` to `rfcs/<your-slug>.md`. Alternatively, load the `write-orkia-rfc` skill in Claude Code (or any CLI agent) and let it scaffold the file.
3. Fill the frontmatter and the eight sections. In *Approach*, cite the files you intend to touch with `path:line` references.
4. Run `script/rfc-lint rfcs/<your-slug>.md` and fix any reported issues before pushing.
5. Open a PR with the `rfc:draft` label. Discussion happens on the PR; revisions go on the same branch.
6. After merge, implementation PRs follow. Each implementation PR references the RFC by path and addresses a subset of the *Tasks* checklist.

## What reviewers look for

- *Constraints* names the `CLAUDE.md` principles that apply and explains how the design respects them. Silence here invites rejection.
- *Approach* cites code with `path:line`. Hand-wavy approaches are returned for revision.
- *Alternatives Considered* contains at least two real alternatives with reasons for rejection. "I didn't consider any" is not an answer.
- *Open Questions* are genuine, not formalities.
- *Acceptance Criteria* are testable. "Works well" is not.

## Where the format comes from

The RFC format used in this repository is the portable variant of Orkia's internal RFC primitive. The same shape — TOML frontmatter, eight body sections, four lifecycle states — works inside Orkia at runtime: RFCs you create in your workspace and RFCs that live in this repo share the same structure. Contributing to Orkia and using Orkia rest on the same primitive, which is the point.

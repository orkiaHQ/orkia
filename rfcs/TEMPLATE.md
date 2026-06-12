+++
# Copy this file to rfcs/<your-slug>.md and fill it in. The slug is
# kebab-case, derived from your issue title (max 64 chars), and must match
# the `id` below. TEMPLATE.md is the only file exempt from that match.
#
# The delimiter is `+++`, not `---`. Frontmatter is TOML. Validate before
# you push: `python3 script/rfc-lint rfcs/<your-slug>.md`.
id          = "replace-with-kebab-case-slug"
title       = "Replace With A Human-Readable Headline"
state       = "draft"
authors     = ["@your-github-handle"]
created_at  = "2026-01-01"
tags        = ["shell"]
priority    = "medium"
# issue     = "GH123"   # uncomment and set if there is a linked issue
+++

## Context

Why this RFC exists. The situation that motivates the change — what hurts
today, and for whom. Link the GitHub issue. Keep it to what a reviewer needs
to understand the problem; save the solution for *Approach*.

## Goals

What "done" looks like, in user-visible outcomes — not tasks. Each goal
should be observable: a thing a user can do, or a property the system holds,
that is not true today.

## Constraints

What the design must respect. Name the `CLAUDE.md` principles that apply, by
heading, and explain how the design honours each. If a principle does not
apply, say so explicitly — a noted-and-dismissed principle reads as care; a
missing one reads as oversight.

- **REPL main loop is sacred** (CLAUDE.md): explain how this design keeps the
  REPL non-blocking, or state why no REPL interaction is involved.
- **One owner per resource** (CLAUDE.md): name the owner of any fd / handle /
  mutable structure this touches, and how others reach it (channel, not
  shared reference).

## Approach

The proposed design. Cite the code you intend to touch with `path/to/file.rs:42`
references — the cited code must actually exist. Describe new modules by the
path they will live at. Hand-wavy approaches are returned for revision.

## Tasks

- [ ] Replace this with the first concrete work unit.
- [ ] One checklist item per coherent chunk of implementation.
- [ ] Implementation PRs check these off as they merge.

## Acceptance Criteria

Testable invariants a reviewer can verify. "Works well" is not testable.

- `cargo clippy --workspace -- -D warnings` passes on the affected crates.
- A concrete, observable behaviour: e.g. "running `orkia` and doing X produces
  Y", verified by a named test or a real-agent demo scenario.

## Alternatives Considered

At least two genuine alternatives, each with the reason it was rejected.
"I didn't consider any" is not an answer.

- **Alternative A.** What it is, and why it loses to the chosen approach.
- **Alternative B.** What it is, and why it loses to the chosen approach.

## Open Questions

Must be non-empty. Genuine forks where reviewer input would change what you
build — not formalities like "are there edge cases?".

- A real, unresolved question where two reasonable reviewers could disagree,
  and the disagreement would change the design. Replace this before pushing.

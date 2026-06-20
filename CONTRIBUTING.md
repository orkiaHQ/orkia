# Contributing to Orkia

Thank you for your interest in contributing to Orkia.

Contributions to Orkia go through RFCs — short markdown+TOML documents
that capture the contract between contributor and reviewer before any
code is written. This is not bureaucracy; it is how we keep the system
coherent. The full primitive lives in [`docs/rfc-primitive.md`](docs/rfc-primitive.md);
the rest of this file is the contributor-facing TL;DR.

## TL;DR

- **File an issue first.** Describe the problem or proposal.
- **A maintainer labels it `ready-to-rfc`** when it is in scope.
- **Write an RFC** in `rfcs/<slug>.md` using `rfcs/TEMPLATE.md`. The
  `write-orkia-rfc` skill at `.agents/skills/write-orkia-rfc/SKILL.md`
  scaffolds the file with any CLI agent (Claude Code, Codex, Gemini CLI).
- **Open a PR labeled `rfc:draft`** with that one file. CI runs
  `script/rfc-lint`. Maintainers review.
- **After the RFC merges**, open a *separate* PR with the
  implementation, referencing the RFC by path.

## The RFC flow

An RFC is a short markdown+TOML document that describes the *contract*
between a contributor and the reviewers: what changes, why, with what
constraints, how a reviewer will know it works. The implementation lands
later, in a separate PR. Two PRs sounds like overhead; in practice it
saves the time spent debating an approach in code review comments after
the work is already done.

## Filing an issue

Use the templates in `.github/ISSUE_TEMPLATE/` (bug or feature). Keep
the issue focused: one problem, one outcome, one decision. If you are
not sure whether a change needs an RFC, file the issue first and ask.
Small bug fixes that do not change behavior beyond fixing the bug do
not need an RFC.

A maintainer adds the `ready-to-rfc` label when the issue is in scope
and ready for a proposal.

## Writing the RFC

1. Copy `rfcs/TEMPLATE.md` to `rfcs/<slug>.md`. The slug is kebab-case,
   derived from the issue title, max 64 chars.
2. Or, run the `write-orkia-rfc` skill (`.agents/skills/write-orkia-rfc/SKILL.md`)
   from your CLI agent — it does steps 1–7 of the procedure for you,
   including filling frontmatter from the GitHub issue and validating
   with the linter before you commit.
3. Fill the eight mandatory sections, in this exact order:

   | Section                  | What it answers                                        |
   |--------------------------|--------------------------------------------------------|
   | `## Context`             | Why this RFC exists. What hurts today.                 |
   | `## Goals`               | What "done" looks like — outcomes, not tasks.          |
   | `## Constraints`         | What must not break. Which `CLAUDE.md` principles apply. |
   | `## Approach`            | How. Cite files with `path/to/file.rs:42` where useful. |
   | `## Tasks`               | Markdown checklist of concrete work units.             |
   | `## Acceptance Criteria` | Testable invariants a reviewer can verify.             |
   | `## Alternatives Considered` | What you also evaluated, with reasons rejected.    |
   | `## Open Questions`      | Genuinely unresolved questions. Must be non-empty.     |

4. Run `python3 script/rfc-lint rfcs/<slug>.md` and fix every error.
   The linter checks frontmatter, section presence, section order, that
   `## Tasks` has at least one checklist item, and that `## Open Questions`
   is non-empty. CI runs the same linter on every PR that touches
   `rfcs/**/*.md`.

## Lifecycle on GitHub

Each RFC maps to a state in its frontmatter and to a label on its PR.

| RFC `state`     | GitHub label       | What it means                                                 | Merge action on the RFC PR       |
|-----------------|--------------------|---------------------------------------------------------------|----------------------------------|
| `draft`         | `rfc:draft`        | Under discussion. Author iterates in response to review.      | Squash-merge when consensus.     |
| `active`        | `rfc:active`       | Merged, implementation in flight.                             | Author bumps `state` in a follow-up commit on merge. |
| `completed`     | `rfc:completed`    | All Acceptance Criteria met; implementation PRs merged.       | Bump `state`, move file under `rfcs/completed/`.     |
| `abandoned`     | `rfc:abandoned`    | Dropped. Document the reason in `## Context`.                 | Bump `state` and move under `rfcs/abandoned/` (we keep abandoned RFCs as historical record). |

`draft` is the contributor-friendly alias for the internal canonical
state `draft-active` (see [`docs/rfc-primitive.md`](docs/rfc-primitive.md)).
The public repo only sees the four labels above.

## Implementation PRs

After an RFC merges:

- Open a *separate* PR with the implementation. Title:
  `<type>: <short description>` per the commit format
  (`feat`, `fix`, `perf`, `refactor`, `test`, `docs`, `chore`).
- Reference the RFC by path in the PR body:
  *"Implements `rfcs/<slug>.md`. Tasks: 1, 2, 3."*
- Implementation may be split across multiple PRs, one per coherent
  chunk of the `## Tasks` checklist. Check items off in the RFC file as
  PRs merge.
- If the implementation reveals the RFC is wrong, *amend the RFC first*
  in its own PR. Do not silently diverge.

## Review

Reviewers (currently two: `@killix`, plus rotating help) look for:

- Every `Acceptance Criteria` line is testable.
- `Constraints` addresses the relevant `CLAUDE.md` principles.
- `Approach` cites real files; the cited code actually exists.
- `Alternatives Considered` lists at least one real rejected option.
- `Open Questions` is honest (two or more non-trivial items).

Expected turnaround: 3–5 business days for first review. Orkia is a
small team and we will not pretend otherwise. If your PR has been
silent for a week, leave a comment and we will respond.

## The principles

Every Orkia design decision is evaluated through five non-negotiable
rules, captured in `CLAUDE.md`. Every RFC's `Constraints` section must
address the principles that apply:

- **REPL sanctity** — the REPL loop never blocks on I/O other than user
  input. Side effects happen on dedicated threads. Event draining runs
  every iteration.
- **One owner per resource** — every file descriptor, process handle,
  and mutable data structure has exactly one owner. Others communicate
  via channels, not shared references.
- **No band-aids on structural problems** — if the architecture is
  wrong, rewrite the component. Do not patch around it.
- **Treat every byte as untrusted** — PTY, socket, and stdin bytes are
  parsed defensively. Malformed input never crashes the shell.
- **Agents run in interactive TUI mode only** — never in print/headless
  mode. Print mode bypasses hooks, breaks tell/attach, and
  fragments the model. The TUI is the contract.

## Code quality (for implementation PRs)

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

All three must pass. No `.unwrap()` or `.expect()` in non-test code.
No `#[allow(...)]` without a justification comment. Functions ≤ 50
lines, module files ≤ 600 lines, hard limit of 4 arguments per
function (use config structs / builders beyond that).

> **Release builds need `ORKIA_KERNEL_PUBKEY_HEX`.** Debug builds
> (the commands above, and `cargo build`) compile and test without it —
> `orkia-kernel-trust` bakes in a `DEV_PUBKEY_HEX` fallback. A `--release`
> build or test **fails to compile** (`E0080`) unless you set the
> production kernel public key, by design (SEC-003 — no dev key ships in a
> release artifact):
>
> ```bash
> export ORKIA_KERNEL_PUBKEY_HEX=<64-hex-char ed25519 public key>
> cargo build --release        # otherwise: E0080 "production build requires …"
> ```
>
> For local `--release` verification you can export any valid 32-byte hex
> key; SEAL chains signed under it just won't verify against the real
> kernel. Source: `crates/orkia-kernel-trust/src/lib.rs`.

## Commit format

```
<type>: <short description>

Types: feat, fix, perf, refactor, test, docs, chore
```

## License

Orkia is licensed under the **Elastic License 2.0**
(`Elastic-2.0`); see [`LICENSE`](LICENSE).

By contributing, you agree your contributions are licensed under the
same Elastic License 2.0. You retain copyright.

- **You can** read, fork, modify, and use Orkia for any internal
  purpose; contribute PRs and be credited; build plugins and tools.
- **You cannot** fork Orkia and sell it as a competing agentic shell
  product.

Use of the "Orkia" name, logos, and marks is governed by
[`TRADEMARK.md`](TRADEMARK.md).

## E2E Testing Gate

Orkia uses a strict E2E gate to validate that contributions don't break
critical user flows. Before opening a PR, run the gate locally:

### Quick check (local mode, in-process backend, ~30s)

```bash
cargo run -p orkia-check -- --mode local
```

### Full check (docker-compose, ~3 minutes)

```bash
# Boot the test stack
docker compose -f docker-compose.test.yml up -d

# Wait for backend health
until curl -fs http://localhost:8080/health; do sleep 1; done

# Run the gate
cargo run -p orkia-check -- --mode compose --json | tee result.json

# Tear down
docker compose -f docker-compose.test.yml down -v
```

### For AI agents contributing

If you are an AI agent helping a contributor, follow this loop:

1. Run `cargo run -p orkia-check -- --mode compose --json > result.json`
2. Parse `result.json` — look at `status` (must be `pass`) and any `failures[]`
3. For each failure, the `failure.hypothesis` field points to the probable fix area
4. Apply fixes, return to step 1
5. Only ping the human when `result.json` shows `status: pass`

The gate is **strict**: no `#[ignore]`, no fail-soft, no env-gated skip.
A passing gate is a real signal, not a placebo. If a flow fails on your
machine but passes elsewhere, file an issue — it's a flakiness bug
that the project considers a blocker.

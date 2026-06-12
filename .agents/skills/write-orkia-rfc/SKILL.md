---
name: write-orkia-rfc
description: Scaffold and complete an Orkia public-repo RFC from a GitHub issue. Use when a contributor asks you to "open an RFC", "write an RFC for issue #N", or to propose any change beyond a trivial fix.
---

# write-orkia-rfc

Compose a portable Orkia RFC in `rfcs/<slug>.md` that addresses a specific
GitHub issue, validates locally via `script/rfc-lint`, and opens as a PR
ready for human review.

The RFC is the **contract** between the contributor and the reviewers. It
does not contain code — it describes intent, constraints, and
acceptance criteria. Implementation lands in a separate PR after the RFC
is merged.

## Inputs

- **Required:** the GitHub issue number (e.g. `123`). If the user did not
  give one, stop and ask. Do not invent one.
- **Optional:** a short hint about the proposed approach. Use it when
  drafting the `## Approach` section; never treat it as the contract.

## Procedure

Seven steps. Do not skip. Each step has an explicit halt condition.

1. **Read the issue.** Run `gh issue view <N> --json title,body,labels`.
   If the issue does not exist or is closed, halt and tell the user.
   Capture the title, body, and labels — you will need all three.

2. **Derive the slug.** Lowercase the title, replace runs of
   non-alphanumeric characters with `-`, strip leading/trailing `-`,
   truncate at 64 characters. Examples:
   - `"Add PKCE for mobile auth"` → `add-pkce-for-mobile-auth`
   - `"Fix REPL deadlock when stdin closes"` → `fix-repl-deadlock-when-stdin-closes`

3. **Copy the template.** `cp rfcs/TEMPLATE.md rfcs/<slug>.md`. If
   `rfcs/<slug>.md` already exists, **halt** and ask the user — do not
   silently overwrite. Two contributors racing on the same slug is a
   coordination problem, not a tooling problem.

4. **Fill the frontmatter.** Edit `rfcs/<slug>.md`'s `+++ ... +++` block:
   - `id = "<slug>"`
   - `title = "<exact issue title>"`
   - `state = "draft"`
   - `authors = ["@<gh-username>"]` — derive from `gh auth status`. If
     `gh auth status` fails or the user is not logged in, ask the user
     for their handle.
   - `created_at = "<today's date as YYYY-MM-DD>"`
   - `tags = []` (or copy meaningful labels from the issue, lowercase,
     stripped of `kind:` / `area:` prefixes)
   - `issue = "GH<N>"` — the issue number you started from.
   Do not add any other field — the linter rejects daemon-managed fields
   like `content_hash`, `version`, `updated_at`, `locked_by`, `agents`.

5. **Write the body.** Eight sections, in order, with substantive content
   — not placeholders. Per-section guidance:

   - **Context.** Two to four sentences. Why now. Link the issue
     (`Addresses #N.`). Cite specific files with `path/to/file.rs:42`
     when prior code is the motivation.
   - **Goals.** Three to five outcome bullets. Not tasks — outcomes.
   - **Constraints.** Enumerate the `CLAUDE.md` principles that apply
     (REPL sanctity, one-owner-per-resource, fail-closed, never-panic,
     interactive-TUI-only for agents, etc.). For each one, state how
     this RFC obeys it. If a principle does not apply, write
     "N/A — does not touch …" so the reviewer knows you considered it.
   - **Approach.** Cite the files you intend to touch with `path:line`
     references where possible. Sketch new types or signatures in a
     fenced code block if it helps. Keep it short — the contract, not
     the patch.
   - **Tasks.** Markdown checklist. Each item small enough to land as
     one commit or one PR. Order them in dependency order.
   - **Acceptance Criteria.** Testable invariants. Prefer commands a
     reviewer can run (`cargo test -p orkia-shell tests::repl::…`) or
     behaviors a reviewer can observe (`running `orkia` in a real
     terminal, typing X produces Y`). Avoid "should work" / "is
     robust" — unfalsifiable.
   - **Alternatives Considered.** At least one real alternative with
     the reason it was rejected. "I didn't consider any" is not
     acceptable.
   - **Open Questions.** At least two non-trivial questions. If you
     have none, you have not thought hard enough — go re-read the
     issue and the affected code paths.

6. **Validate.** Run `python3 script/rfc-lint rfcs/<slug>.md`. If it
   exits non-zero, **fix every error before continuing**. Do not open
   a PR with linter failures; CI will reject it anyway and you will
   have wasted a reviewer's notification.

7. **Open the PR.** Stage the single file, commit with
   `rfc: <issue title>`, push to a branch named `rfc/<slug>`, and run:
   ```
   gh pr create \
     --title "rfc: <issue title>" \
     --body "Addresses #<N>. See rfcs/<slug>.md for the contract." \
     --label "rfc:draft"
   ```
   Do not include code changes in this PR. Implementation is a
   separate PR after the RFC merges (see CONTRIBUTING.md "Lifecycle
   on GitHub").

## Validation

After step 6, the RFC passes validation if and only if:

- `python3 script/rfc-lint rfcs/<slug>.md` exits 0.
- All eight required sections contain substantive content, not just
  the `<...>` placeholders from the template.
- `## Open Questions` lists two or more non-trivial questions.
- `## Constraints` explicitly addresses the relevant `CLAUDE.md`
  principles (REPL sanctity, one-owner, fail-closed, never-panic,
  interactive-TUI-only).
- `## Acceptance Criteria` items are testable invariants, not
  aspirations.

## Anti-patterns

Do not do any of the following. They are the failure modes that make
reviewers tired and RFCs hated.

- **Writing the RFC after the code.** If implementation already exists
  in a local branch, declare it explicitly in `## Context`
  ("Prototype lives at branch `…`; this RFC retroactively documents
  it for review."). Hidden code-first work loses reviewer trust.
- **Empty or perfunctory `Alternatives Considered`.** A single bullet
  saying "do nothing" does not count. Find one real architectural
  alternative and explain why you rejected it.
- **Vague Acceptance Criteria.** "The feature works" is not testable.
  "`cargo test -p orkia-shell tests::repl::reattach_after_detach`
  passes" is.
- **Hand-wavy Approach.** "Refactor the engine" without citing files
  is not an approach; it is a wish.
- **Fewer than two Open Questions.** This is a smell. Edge cases
  exist. Failure modes exist. Tradeoffs exist. Find them.
- **Daemon-managed fields in frontmatter.** `content_hash`, `version`,
  `updated_at`, `locked_by`, `locked_at`, `agents` — these belong to
  the Orkia daemon, not to public-repo RFCs. The linter rejects them.
- **Overwriting an existing slug.** If `rfcs/<slug>.md` exists, stop
  and ask the user. Two contributors racing on the same RFC is a
  coordination problem, not a tooling one.
- **Mixing RFC and implementation in one PR.** The RFC PR contains
  one file: `rfcs/<slug>.md`. Implementation is a separate PR.

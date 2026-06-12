# Engineering Principles

Orkia is a shell. It replaces bash/zsh. It runs 24/7. It hosts every agent, every command, every workflow.

If it crashes, the user loses their session.
If it deadlocks, the user is locked out of their terminal.
If it corrupts a PTY, the agent is gone.

**This is not an app. This is infrastructure.** Every decision is judged against that.

---

## The Non-Negotiables

These are not preferences. They are the load-bearing walls.

### 1. The REPL loop is sacred

The REPL reads input, classifies, dispatches, drains events, renders, loops. It is the heartbeat of the shell.

- Never block on I/O other than user input.
- Side effects (PTY writes, network, file I/O, hypervisor calls) happen in dedicated threads.
- Drain events on every iteration — not just before prompts.
- If a feature requires the REPL to be "awake," the feature is architecturally wrong. Move it to a background thread.

### 2. One owner per resource

Every fd, every process handle, every mutable structure has exactly one owner. Other components communicate by **message**, not shared reference.

| Resource | Owner |
|---|---|
| PTY master fds | `PtyWriteExecutor` thread |
| Alacritty `Term` grid | Engine reader thread |
| Journal store | Journal thread (via socket) |
| Agent job state | REPL thread (`JobController`) |

If you reach for `Arc<Mutex<T>>` on a core data structure, stop. Ask whether a channel removes the sharing entirely. The answer is almost always yes.

### 3. Durability over speed of implementation

A `Mutex` shortcut today is a deadlock in six months when eight call sites compete for the same lock. A dedicated channel costs thirty extra minutes now and eliminates a class of bugs permanently. Pay the cost.

### 4. No band-aids on structural problems

The attach pump was patched six times before we recognized that crossterm event parsing on the attach path was the structural bug. The fix was deleting the pump and rewriting as a raw byte splice. Every patch before that was wasted work.

You're patching, not fixing, when:

- The fix adds a special case or a flag.
- The fix works around a behavior instead of changing it.
- The fix needs "just one more" workaround.
- You've touched this area more than twice for related bugs.

Stop. Diagnose the structural problem. Write the real fix.

### 5. Agents run in interactive (TUI) mode. Never in print/headless mode.

Orkia never spawns an agent using `claude -p`, Codex `exec`, Gemini `--prompt`, or any "one answer and exit" flag. The only execution model for an agent under Orkia is the full interactive TUI session driven over a PTY.

This is absolute. Single dispatch (`@faye`), pipes (`cat file | @faye`), agent-to-agent (`@a | @b`), RFC delegation, scheduled runs — all interactive. Always.

Why:

- **Agents are sessions, not function calls.** Two execution paths means two sets of bugs, two test matrices, two mental models. Print mode collapses an agent to a one-shot RPC and breaks `tell`, `attach`, the state machine, and SEAL.
- **Hook coverage is the whole approval story.** PreToolUse / PermissionRequest / PostToolUse / Stop are how Orkia mediates approvals and records SEAL evidence. Print mode bypasses or reorders these.
- **The TUI is the contract.** Permission dialogs, plan mode, trust prompts, vim-inside-claude, Ctrl-C / Ctrl-Z — they only exist interactively.
- **Print mode is provider-shaped.** Standardizing on it imports three vendor surfaces into Orkia's core. Standardizing on TUI standardizes on what humans actually use.

If you're reaching for print mode, you're solving the wrong problem. The real problem is **content capture from an interactive session** — solve that via hook payloads, transcript readers, or a structured "final response" channel.

### 6. Test with real agents, not mocks

Claude Code, Codex, and Gemini are TUI programs with their own terminal handling, signal handling, and escape sequences. Mocks hide the real bugs.

The acceptance criteria for attach mode, prompt detection, and injection are not "unit test passes." They are: claude's trust prompt works, vim inside claude works, Ctrl-C reaches claude, Ctrl-Z detaches cleanly.

### 7. Treat every byte as untrusted

Bytes from a PTY, the journal socket, stdin, an LLM response, an MCP server — all untrusted. Parse defensively. Handle malformed sequences gracefully. Never panic on unexpected input. A malformed escape sequence from an agent must never crash the shell.

### 8. Fail-closed by default

- Unknown policy → deny.
- Trust uncomputable → lock the agent.
- Audit write failure → abort the tool call.
- SEAL chain hash mismatch → mark corrupted, surface to user.
- Malformed PTY escape → log, continue, never panic.

---

## How We Work

### Think before coding

Don't assume. Don't hide confusion. Surface tradeoffs.

- State assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them — don't pick silently.
- If a simpler approach exists, say so.
- If something is unclear, stop. Name what's confusing. Ask.

### Simplicity first

Minimum code that solves the problem. Nothing speculative.

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you wrote 200 lines and it could be 50, rewrite it.

The test: would a senior engineer call this overcomplicated? If yes, simplify.

### Surgical changes

Touch only what you must. Clean up only your own mess.

When editing existing code:

- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it — don't delete it.

When your changes orphan code: remove imports/variables/functions **your changes** made unused. Don't touch pre-existing dead code unless asked.

Every changed line should trace directly to the request.

### Goal-driven execution

Transform tasks into verifiable goals:

- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state the plan up front:

```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work") cost everyone time.

---

## Code Quality

### Zero warnings

```bash
cargo clippy --workspace -- -D warnings   # must pass before every commit
cargo fmt --all
```

Enforce in non-test code:

```rust
#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
```

No `#[allow(...)]` without a justification comment.

### Errors

- Return `Result<T, E>` with crate-specific error types via `thiserror`.
- Libraries: never `unwrap`, `expect`, or `panic` outside tests.
- Binaries (CLI, shim): `anyhow` is fine. Libraries: not.

### Size limits

| Scope | Max | Action when exceeded |
|---|---|---|
| Module file | 600 lines | Split into submodules |
| Function | 50 lines | Extract helpers |
| `impl` block | 200 lines | Split into trait impls |
| Test file | 500 lines | Split into focused modules |
| Function arguments | 4 | Use a config struct or builder |

### Style

- Favor immutability, borrowing over cloning, builders over `new`.
- Prefer standalone functions over `&self` methods when there's no real state.
- Builder pattern for structs; RAII for resources.
- Composition over inheritance (traits, not struct embedding).

### Avoid

- `Box` / `Pin` / `Arc` wrapping when simpler ownership works.
- Global state (`OnceLock<Mutex<HashMap<...>>>`).
- Mixed responsibilities in a single module.
- Redundant allocations during type conversions.

### Commits

```
<type>: <short description>
```

Types: `feat`, `fix`, `perf`, `refactor`, `test`, `docs`, `chore`.

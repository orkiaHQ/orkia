# orkia-difficulty-scorer

Standalone difficulty scoring for prompt complexity analysis.

## Overview

`orkia-difficulty-scorer` extracts structural and language-level
signals from a user prompt and reduces them to a single scalar in
`[0, 1]` — high values mean "this looks like a hard, complex
request that benefits from a frontier model"; low values mean
"this is short, plain, doesn't need much horsepower". The score
feeds the agent router and the model-tier selection logic, but
the crate itself has no dependency on either: it is pure
computation over a `&str` and exposes the intermediate signals
for callers that want to do something else with them.

The crate has zero dependencies (not even `serde`) by design.
It can be lifted into any other Orkia component, into a benchmark,
or extracted as a standalone library without ripple effects.

## Modules

- `structural` — `extract_structural(prompt)` and
  `StructuralSignals`: token count, character count, unique-token
  ratio, code-block / inline-code / code-line counts, nesting
  depth, URL / newline / list-marker counts, punctuation density,
  indentation levels, presence of tables / JSON / XML, math
  symbol count, parenthetical depth, quoted-block count, section
  count.
- `language_detect` — `detect_english(prompt)`: very small
  heuristic English detector. Returning `false` skips the English
  boost computation.
- `english_boost` — `extract_english_boost(prompt)` and
  `EnglishBoostSignals`: language-level signals that only make
  sense for English text.
- `scorer` — `DifficultyScorer`, `DifficultyWeights`. Default
  weights distribute mass across length, code, structure,
  vocabulary, formatting, math, context, and the English boost.

The crate also defines `ModelTier` (`Economy` / `Performance` /
`Frontier`) and the umbrella `DifficultySignals` struct produced
by `DifficultySignals::extract(prompt)`.

## Public API surface

```rust
use orkia_difficulty_scorer::{DifficultyScorer, DifficultySignals};

let prompt = "refactor src/repl.rs to extract the event drain loop";
let signals = DifficultySignals::extract(prompt);
let scorer = DifficultyScorer::new();
let score: f32 = scorer.score(&signals);   // 0.0 ..= 1.0
```

Custom weighting:

```rust
use orkia_difficulty_scorer::{DifficultyScorer, DifficultyWeights};

let weights = DifficultyWeights {
    w_code: 0.30,
    w_length: 0.10,
    ..DifficultyWeights::default()
};
let scorer = DifficultyScorer::with_weights(weights);
```

## Consumed by

- The agent router in `orkia-shell` (when promoted from
  heuristic mode) and tier-selection logic in the model-catalog
  consumers. The crate currently stands on its own; callers wire
  it in where they need it.

## Development notes

- Zero external dependencies. Everything is in-crate Rust.
- `deny(warnings)`, `deny(clippy::unwrap_used)`,
  `deny(clippy::expect_used)`.
- The crate is small enough to read end-to-end; benchmarks can
  drive it directly with raw strings.

## License

`Elastic-2.0` (per the workspace `Cargo.toml`).

# orkia-model-catalog

Canonical model capability catalog — shared seed data for model
routing.

## Overview

`orkia-model-catalog` holds the reference set of LLMs Orkia knows
about, the per-intent quality scores it uses to rank them, and
the cost / context-window / capability metadata that the router
and tier-selection logic consult. The crate is essentially a
typed seed file: a small set of struct definitions plus a `seed()`
function returning the built-in catalog.

Keeping this in a leaf crate means the seed list can be updated
in isolation (a new model release is a one-PR change to one
file) without forcing rebuilds of unrelated subsystems, and means
servers, the shell, and any offline tools share the exact same
table.

## Modules

- `types` — `ModelProfile` and `IntentQuality`.
- `seed` — `seed()`: returns a `Vec<ModelProfile>` with the
  built-in reference catalog (Anthropic, OpenAI, Google, and
  other providers; tiers `frontier` / `performance` / `economy`).

## Public API surface

```rust
use orkia_model_catalog::{seed, ModelProfile, IntentQuality};

let catalog: Vec<ModelProfile> = seed();
for m in &catalog {
    println!("{} {} ${}", m.model_id, m.tier, m.input_cost_per_m);
}
```

`ModelProfile` carries:

- `model_id`, `provider`, `tier` (`"frontier" | "performance" | "economy"`)
- `context_window` (tokens)
- `input_cost_per_m`, `output_cost_per_m` (USD per million tokens)
- `supports_vision`, `supports_tools`
- `intents: Vec<IntentQuality>` — per-intent quality score in
  `[0, 1]` for intents like `code_generation`, `reasoning`,
  `code_review`, `translation`, `summarization`, `classification`,
  `extraction`.

Both types derive `Serialize` / `Deserialize`, so the catalog can
be round-tripped through JSON for tooling and tests.

## Consumed by

- The router / tier-selection logic in `orkia-shell` (when it
  picks the model to run an intent against).
- Any tooling that needs the canonical model metadata without
  hitting a network.

## Development notes

- Single dependency: `serde`. No I/O, no async, no networking.
- The seed function deliberately returns owned `Vec<ModelProfile>`
  rather than a `&'static` table — call sites that need a static
  copy should `Lazy`-cache the result themselves.

## License

`Elastic-2.0` (per the workspace `Cargo.toml`).

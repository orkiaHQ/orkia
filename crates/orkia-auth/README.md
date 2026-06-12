# orkia-auth

Neutral authentication primitives for the Orkia shell. Two layers, both backend-agnostic:

- **`store`** — OS-level secret storage. `TokenStore<M>` trait plus `KeyringStore<M>` (OS keychain via the `keyring` crate) and `FileStore<M>` (mode-0600 file fallback at `~/.orkia/auth.toml`). Generic over the metadata payload `M` — the trait stores opaque `(token, M)` pairs and never interprets the contents. `default_store` honours `ORKIA_SESSION_FILE`: when set, it forces a `FileStore` at that path (headless / CI sessions write a real backend session there); otherwise it uses the OS keychain, then the `~/.orkia` file fallback.
- **`provider`** — `AuthProvider` trait, the seam between the shell and a concrete auth backend. Concrete implementations live elsewhere.

Default keychain service name: `dev.orkia.cli` (when an impl uses `default_store`).

## Implementations of `AuthProvider`

There is no env-injected provider — a session comes only from a real backend login, persisted via `store`. Reads (`current`/`bearer`) load that persisted session; there is no way to assert a plan from the environment.

- **`MagicLinkAuthProvider`** ships in the public `orkia-magic-login` crate. Runs the email → one-time-code → bearer flow against the configured backend and persists the signed-JWT session (with its plan claim) through `store`. The OSS `orkia` binary wires this.
- **`OrkiaAuthProvider`** lives in the proprietary distribution. Wraps `OAuthClient` + `TokenStore<TokenMetadata>` to drive the full `/v1/auth/cli/*` flow against `api.orkia.io`. The shell consumes it through `Arc<dyn AuthProvider>` and never names it directly.

## Boundary

This crate intentionally has no knowledge of any specific backend URL, JWT shape, or claim names. New proprietary auth flows add their own `AuthProvider` impl in a downstream crate; they do not modify `orkia-auth`.

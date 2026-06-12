// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `orkia-value` — the wasm-compatible heart of the value model.
//!
//! Holds `Value`, `Type`, and the tagged-JSON boundary bridge
//! (`value_to_json` / `json_to_value`). These are lean and dependency-light
//! (serde / serde_json / chrono-no-clock / indexmap / base64), so the **same**
//! types and the **same** wire format are shared by:
//!   - the host (`orkia-shell-types` re-exports `Value`/`Type`; `orkia-plugin`
//!     re-exports the bridge), and
//!   - the plugin SDK (`orkia-plugin-sdk`, compiled to `wasm32-wasip1`).
//!
//! They live here rather than in `orkia-shell-types` because that crate pulls
//! native-only deps (alacritty / pty / libc via `orkia-terminal-core`) and so
//! cannot build for wasm. One `Value`, one bridge, no drift.
#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod bridge;
mod typ;
mod value;

pub use bridge::{json_to_value, value_to_json};
pub use typ::Type;
pub use value::Value;

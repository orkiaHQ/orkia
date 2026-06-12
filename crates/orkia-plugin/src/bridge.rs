// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! The implementation moved to the shared, wasm-buildable `orkia-value` crate
//! so the host and **every** plugin SDK (TS via `@orkia/value`, Rust via
//! `orkia-plugin-sdk`) encode/decode the *exact same* tagged JSON — the
//! host can't tell which language a plugin was written in. Re-exported here so
//! `crate::bridge::{value_to_json, json_to_value}` call sites are unchanged.

pub use orkia_value::{json_to_value, value_to_json};

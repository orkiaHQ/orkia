// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! The pipeline type system (`Type`) and the contract-check primitive.
//!
//! Moved to the lean, wasm-buildable `orkia-value` crate (alongside `Value`)
//! and re-exported here so every existing `orkia_shell_types::exec::typ::Type`
//! / `orkia_shell_types::Type` path is unchanged.

pub use orkia_value::Type;

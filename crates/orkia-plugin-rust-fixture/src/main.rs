// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Keeps table rows whose `size` (a `Filesize`) is ≥ the `--min_size` argument.
//! The `#[orkia::command]` macro generates the `wasm32-wasip1` entry glue.
use orkia_plugin_sdk::prelude::*;

#[orkia::command]
fn where_big(input: Vec<Value>, args: Args) -> Result<Vec<Value>> {
    let min: i64 = args.get("min_size")?;
    Ok(input
        .into_iter()
        .filter(|row| matches!(row.get_path("size"), Some(Value::Filesize(n)) if *n >= min))
        .collect())
}

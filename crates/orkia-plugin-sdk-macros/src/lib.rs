// Copyright 2026 Orkia
// SPDX-License-Identifier: Apache-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Apache License 2.0; see https://www.apache.org/licenses/LICENSE-2.0
// for terms.

//!
//! Annotates the plugin's transform function and generates the WASM entry glue:
//! a `main` (the `wasm32-wasip1` `_start`) that reads the input envelope
//! from stdin, runs the function, and writes the output to stdout — the
//! Rust counterpart of PLUGIN-TS's JS bridge. The serialization itself lives in
//! `orkia_plugin_sdk::run_command` (which reuses the shared `orkia-value`
//! bridge), so this macro stays a thin, debuggable shim.
#![deny(warnings)]

use proc_macro::TokenStream;
use quote::quote;
use syn::{ItemFn, parse_macro_input};

/// Mark a `fn(Vec<Value>, Args) -> Result<Vec<Value>>` as the plugin's command.
/// Generates the `main` that drives the host↔guest boundary. One command per
/// plugin crate (one `.wasm` = one command), matching PLUGIN-TS.
#[proc_macro_attribute]
pub fn command(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);
    let name = &func.sig.ident;
    let expanded = quote! {
        #func

        // wasm32-wasip1 entry point (`_start`): read the input envelope
        // from stdin, run the command, write the output to stdout.
        fn main() {
            ::orkia_plugin_sdk::run_command(#name);
        }
    };
    expanded.into()
}

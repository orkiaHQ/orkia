// Copyright 2026 Orkia
// SPDX-License-Identifier: Apache-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Apache License 2.0; see https://www.apache.org/licenses/LICENSE-2.0
// for terms.

//!
//! Transforms a TS/JS plugin *source* into a pre-compiled QuickJS-WASM
//! `.cwasm` module the runtime loads. Shipped as a **separate artifact**
//! (`orkia-compiler` binary), pulled on demand — never linked into the default
//! `orkia` binary, since it carries OXC + Cranelift (~2 MB+ each) that
//! the ~80% who only *consume* `.cwasm` plugins never need.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod bundle;
pub mod compile;
pub mod error;
pub mod pull;
pub mod transpile;

pub use compile::compile_file;
pub use error::CompileError;
pub use pull::ensure_javy;
pub use transpile::transpile_ts;

// Copyright 2026 Orkia
// SPDX-License-Identifier: Apache-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Apache License 2.0; see https://www.apache.org/licenses/LICENSE-2.0
// for terms.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CompileError {
    #[error("transpile (TS→JS): {0}")]
    Transpile(String),
    #[error("javy (JS→WASM): {0}")]
    Javy(String),
    #[error("precompile (WASM→cwasm): {0}")]
    Precompile(String),
    #[error("compiler pull: {0}")]
    Pull(String),
    #[error("io: {0}")]
    Io(String),
    #[error("bundle (module resolution): {0}")]
    Bundle(String),
}

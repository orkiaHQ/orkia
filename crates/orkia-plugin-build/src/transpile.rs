// Copyright 2026 Orkia
// SPDX-License-Identifier: Apache-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Apache License 2.0; see https://www.apache.org/licenses/LICENSE-2.0
// for terms.

//!
//! QuickJS runs ES2023, not TypeScript, so types are stripped (and modern
//! syntax lowered) before the JS is handed to the QuickJS-WASM compiler. OXC
//! is ~2 MB and prod-proven (Rolldown/Vue); it lives in the compiler artifact,
//! never in the default `orkia` binary.

use std::path::Path;

use oxc::allocator::Allocator;
use oxc::codegen::Codegen;
use oxc::parser::Parser;
use oxc::semantic::SemanticBuilder;
use oxc::span::SourceType;
use oxc::transformer::{TransformOptions, Transformer};

use crate::error::CompileError;

/// Transpile TypeScript source to JavaScript (type-stripped). `path` drives
/// the source-type inference (`.ts`/`.tsx`/`.mts`).
pub fn transpile_ts(source: &str, path: &Path) -> Result<String, CompileError> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or(SourceType::ts());

    let parsed = Parser::new(&allocator, source, source_type).parse();
    if !parsed.errors.is_empty() {
        let msg = parsed
            .errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(CompileError::Transpile(format!("parse: {msg}")));
    }
    let mut program = parsed.program;

    let scoping = SemanticBuilder::new()
        .build(&program)
        .semantic
        .into_scoping();
    let options = TransformOptions::default();
    let result =
        Transformer::new(&allocator, path, &options).build_with_scoping(scoping, &mut program);
    if !result.errors.is_empty() {
        let msg = result
            .errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(CompileError::Transpile(format!("transform: {msg}")));
    }

    Ok(Codegen::new().build(&program).code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_types() {
        let ts =
            "function add(a: number, b: number): number { return a + b; }\nconst x: string = 'hi';";
        let js = transpile_ts(ts, Path::new("x.ts")).expect("transpile");
        // Type annotations are gone; the runtime code remains.
        assert!(!js.contains(": number"), "type annotations stripped: {js}");
        assert!(js.contains("function add"), "code preserved: {js}");
        assert!(js.contains("return a + b"), "body preserved: {js}");
    }
}

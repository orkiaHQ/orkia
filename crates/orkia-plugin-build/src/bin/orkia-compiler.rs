// Copyright 2026 Orkia
// SPDX-License-Identifier: Apache-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Apache License 2.0; see https://www.apache.org/licenses/LICENSE-2.0
// for terms.

//!
//!   orkia-compiler compile <src.ts|.js> -o <out.cwasm>
//!   orkia-compiler install            # prefetch + verify the QuickJS compiler

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("compile") => compile(&args[1..]),
        Some("install") => match orkia_plugin_build::ensure_javy() {
            Ok(path) => {
                eprintln!("compiler ready: {}", path.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("orkia-compiler: {e}");
                ExitCode::FAILURE
            }
        },
        _ => usage(),
    }
}

fn compile(rest: &[String]) -> ExitCode {
    // compile <src> -o <out>
    let mut src: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-o" | "--out" => out = it.next().map(PathBuf::from),
            _ => {
                if src.is_none() {
                    src = Some(PathBuf::from(arg));
                }
            }
        }
    }
    let (Some(src), Some(out)) = (src, out) else {
        return usage();
    };
    match orkia_plugin_build::compile_file(&src) {
        Ok(cwasm) => match std::fs::write(&out, &cwasm) {
            Ok(()) => {
                eprintln!(
                    "compiled {} -> {} ({} bytes)",
                    src.display(),
                    out.display(),
                    cwasm.len()
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("orkia-compiler: write {}: {e}", out.display());
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("orkia-compiler: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "usage:\n  orkia-compiler compile <src.ts|.js> -o <out.cwasm>\n  orkia-compiler install"
    );
    ExitCode::FAILURE
}

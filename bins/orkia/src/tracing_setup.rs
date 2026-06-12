// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use std::path::PathBuf;

/// Tracing setup. Honours two env vars:
///
/// * `RUST_LOG=<filter>` — standard env_filter syntax
///   (e.g. `info`, `debug`, `orkia_shell=trace,brush_core=warn`).
/// * `ORKIA_LOG=<path>` — when set, logs are appended to that file
///   instead of stdout. This is what you want in an interactive
///   session: `tail -f $ORKIA_LOG` from another tab without
///   polluting the shell prompt.
///
/// With neither set, the default subscriber writes `INFO+` to stdout.
/// In shell mode that interleaves with command output, so the file
/// route is strongly preferred for live debugging.
pub(crate) fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};

    // Default to `warn`-only when `RUST_LOG` is unset: an interactive
    // shell shouldn't have stray INFO lines drifting into the user's
    // terminal above the welcome banner. Users who want detail set
    // `RUST_LOG=info` or `RUST_LOG=debug` explicitly, typically paired
    // with `ORKIA_LOG=/tmp/orkia.log` so the chatter goes to a file.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));

    if let Some(path) = std::env::var_os("ORKIA_LOG") {
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(file) => {
                fmt()
                    .with_env_filter(filter)
                    .with_ansi(false)
                    .with_writer(std::sync::Mutex::new(file))
                    .init();
                eprintln!("  \x1b[90morkia: tracing → {}\x1b[0m", path.display());
                return;
            }
            Err(e) => {
                eprintln!(
                    "  \x1b[33morkia: ORKIA_LOG={}: {e} — falling back to stderr\x1b[0m",
                    path.display(),
                );
            }
        }
    }

    // No ORKIA_LOG (or open failed): fall back to stderr so logs don't
    // pollute the stdout pipe stream.
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

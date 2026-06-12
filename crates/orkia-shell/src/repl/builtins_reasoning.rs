// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `$reasoning` builtin handler. The read/format work lives in
//! [`crate::reasoning_builtins`]; this file owns the
//! pieces that need `&mut Repl` — namely `purge`, which tears the consumer down,
//! drops the local store, and re-boots (the REPL is the orchestrator so there
//! are never two writers on the SQLite file, CLAUDE.md #2).

use super::*;

impl Repl {
    pub(crate) async fn handle_reasoning(&mut self, args: &[String]) -> Outcome {
        // Bare `reasoning` (no subcommand) prints usage; `reasoning status` is
        // the explicit status surface.
        let Some(sub) = args.first().map(String::as_str) else {
            return Outcome::BuiltinOutput {
                blocks: crate::reasoning_builtins::usage(""),
            };
        };
        let rest = args.get(1..).unwrap_or_default();
        let blocks = match sub {
            "status" => {
                crate::reasoning_builtins::status(self.intelligence.as_ref(), &self.config.data_dir)
            }
            "sync" => crate::reasoning_builtins::sync(self.intelligence.as_ref()),
            "graph" => crate::reasoning_builtins::graph(&self.config.data_dir, rest),
            "recall" => crate::reasoning_builtins::recall(&self.config.data_dir, rest),
            "purge" => self.purge_reasoning(),
            other => crate::reasoning_builtins::usage(other),
        };
        Outcome::BuiltinOutput { blocks }
    }

    /// Drop all locally-captured reasoning data. Order matters: stop the tasks
    /// first (releasing both store connections), delete the SQLite files, then
    /// re-boot so capture resumes clean. Re-boot is a no-op when the gate is
    /// closed (free/anonymous) — purge then just clears whatever was on disk.
    fn purge_reasoning(&mut self) -> Vec<BlockContent> {
        let was_active = self.intelligence.as_ref().is_some_and(|i| i.is_active());
        if let Some(intel) = self.intelligence.as_mut() {
            intel.shutdown();
        }
        let mut out = vec![BlockContent::SystemInfo(" reasoning purge".into())];
        let removed = remove_store_files(&self.config.data_dir);
        out.push(BlockContent::Text(format!(
            "  removed {removed} local store file(s)"
        )));
        if was_active {
            self.reboot_intelligence();
            let live = self.intelligence.as_ref().is_some_and(|i| i.is_active());
            out.push(BlockContent::Text(if live {
                "  ✓ capture restarted".into()
            } else {
                "  capture stopped (re-`login` to resume)".into()
            }));
        }
        out
    }
}

/// Delete `reasoning.db` and its WAL/SHM sidecars. Missing files are not an
/// error (nothing to purge). Returns how many files were actually removed.
fn remove_store_files(data_dir: &std::path::Path) -> usize {
    let base = crate::reasoning_builtins::store_path(data_dir);
    let mut removed = 0;
    for suffix in ["", "-wal", "-shm"] {
        let path = if suffix.is_empty() {
            base.clone()
        } else {
            let mut s = base.clone().into_os_string();
            s.push(suffix);
            std::path::PathBuf::from(s)
        };
        if path.exists() && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

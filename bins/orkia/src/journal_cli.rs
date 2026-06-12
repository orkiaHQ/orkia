// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `orkia journal [filters]` subcommand.
//!
//! Reads `~/.orkia/journal.jsonl` directly and applies the same
//! filter parser the shell builtin uses. Works without a running
//! REPL — useful from scripts, cron, and CI.

use std::path::PathBuf;

use orkia_shell::journal::{JournalStore, ParsedJournalArgs, journal_help_text, query_row};

pub fn run(args: &[String]) -> i32 {
    let parsed = match ParsedJournalArgs::parse(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("orkia journal: {e}");
            eprintln!("{}", journal_help_text());
            return 2;
        }
    };
    if parsed.help {
        println!("{}", journal_help_text());
        return 0;
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        eprintln!("orkia journal: HOME not set");
        return 2;
    };
    let data_dir = home.join(".orkia");
    let store = JournalStore::new(&data_dir);
    let hits = store.query(&parsed.filter);
    if hits.is_empty() {
        eprintln!("no events match");
        return 0;
    }
    println!("timestamp                      type       job   agent         summary");
    for env in hits {
        println!("{}", query_row(env));
    }
    0
}

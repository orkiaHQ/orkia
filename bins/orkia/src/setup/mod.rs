// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `orkia setup` — first-time setup and incremental reconfiguration.
//!
//! Entry points:
//! - [`run_setup`] dispatched from the `setup` subcommand
//! - [`auto_prompt_if_missing`] called once at REPL startup
//!

mod builtins;
mod prompts;
mod registry;
mod scaffold;
mod tools;
mod wizard;

use std::io;
use std::path::{Path, PathBuf};

pub use prompts::ask_yn;

#[derive(Debug, Default, Clone)]
pub struct SetupArgs {
    pub minimal: bool,
    pub force: bool,
    pub offline: bool,
}

impl SetupArgs {
    /// Parse `--minimal` / `--force` / `--offline` after the `setup`
    /// subcommand token. Stays hand-rolled to match `parse_args` in
    /// `main.rs` (the project deliberately avoids clap on the binary).
    pub fn parse<I: IntoIterator<Item = String>>(iter: I) -> Result<Self, String> {
        let mut out = Self::default();
        for a in iter {
            match a.as_str() {
                "--minimal" => out.minimal = true,
                "--force" => out.force = true,
                "--offline" => out.offline = true,
                "-h" | "--help" => return Err("__help__".into()),
                other => return Err(format!("unknown setup flag: {other}")),
            }
        }
        Ok(out)
    }
}

/// Top-level entry. Honors `--force` (wipe), `--minimal` (no wizard),
/// `--offline` (skip registry sync, use cache or builtins).
pub fn run_setup(args: &SetupArgs) -> io::Result<()> {
    let home = home_dir()?;
    let orkia_dir = home.join(".orkia");

    if args.force && orkia_dir.exists() {
        eprintln!("  --force: removing existing {} ...", orkia_dir.display());
        std::fs::remove_dir_all(&orkia_dir)?;
    }

    let is_fresh = !orkia_dir.exists();
    scaffold::create_base_dirs(&orkia_dir)?;

    if args.minimal {
        minimal_summary(&orkia_dir);
        return Ok(());
    }

    if is_fresh || args.force {
        wizard::fresh_setup(&home, &orkia_dir, args);
    } else {
        wizard::relaunch_menu(&home, &orkia_dir, args);
    }
    Ok(())
}

/// Called at shell startup before the REPL. If `~/.orkia/` exists,
/// returns without prompting. Otherwise asks the user whether to run
/// the full wizard; on decline, creates the bare directory structure so
/// the shell can boot without surprises.
pub fn auto_prompt_if_missing() -> io::Result<()> {
    let Ok(home) = home_dir() else { return Ok(()) };
    let orkia_dir = home.join(".orkia");
    if orkia_dir.exists() {
        return Ok(());
    }
    eprintln!();
    eprintln!(
        "  \x1b[35m⬡\x1b[0m orkia is not configured yet ({} is missing).",
        orkia_dir.display()
    );
    if ask_yn("run setup now?", true) {
        run_setup(&SetupArgs::default())
    } else {
        scaffold::create_base_dirs(&orkia_dir)?;
        eprintln!(
            "  ok — minimal {} created. run `orkia setup` later.",
            orkia_dir.display()
        );
        Ok(())
    }
}

/// Print the brief help block for `orkia setup`.
pub fn print_help() {
    eprintln!(
        "Usage: orkia setup [OPTIONS]

OPTIONS:
    --minimal    Create ~/.orkia/ with empty defaults, no wizard
    --force      Wipe existing ~/.orkia/ and re-run the wizard
    --offline    Skip the archetype registry sync (use cache or builtins)
    -h, --help   Print this help"
    );
}

fn minimal_summary(orkia_dir: &Path) {
    eprintln!("  ✓ {} created (minimal)", orkia_dir.display());
    eprintln!("  run `orkia setup` later for the interactive wizard.");
}

fn home_dir() -> io::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("$HOME is not set"))
}

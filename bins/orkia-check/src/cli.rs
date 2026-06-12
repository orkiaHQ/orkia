// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! `orkia-check` CLI surface (clap derive).
//!

use clap::{Parser, ValueEnum};

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum ModeArg {
    Local,
    Compose,
}

#[derive(Debug, Parser)]
#[command(
    name = "orkia-check",
    version,
    about = "Run the Orkia E2E full-stack gate"
)]
pub struct Cli {
    /// Execution mode.
    #[arg(long, value_enum, default_value_t = ModeArg::Compose)]
    pub mode: ModeArg,

    /// Emit machine-readable JSON to stdout.
    #[arg(long)]
    pub json: bool,

    /// Run only flows whose id contains this substring.
    #[arg(long)]
    pub filter: Option<String>,

    /// List all flows and exit.
    #[arg(long)]
    pub list: bool,

    /// Per-flow timeout in seconds.
    #[arg(long, default_value_t = 60)]
    pub timeout_flow: u64,

    /// Total timeout in seconds.
    #[arg(long, default_value_t = 600)]
    pub timeout_total: u64,

    /// Verbose human output (ignored with --json).
    #[arg(long)]
    pub verbose: bool,
}

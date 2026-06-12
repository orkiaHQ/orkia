// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `orkia-cage` — the OSS execution-boundary launcher binary.
//!
//! A thin shim over the [`orkia_cage`] library: init logging, parse args, and
//! run with **no** trust adjuster (`None` → `NoopTrustAdjuster`, inert — V1
//! behaviour is byte-identical to before the Trust Atlas seam existed). The
//! enterprise launcher is a *separate* binary that passes its scoring adjuster;
//! it cannot bypass `apply_trust`, only supply an `AskOutcome`.

use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    // Logs go to stderr so they never corrupt the agent's stdout on the PTY.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // OSS build: no adjuster registered → the cage stays inert.
    orkia_cage::run(orkia_cage::Args::parse(), None)
}

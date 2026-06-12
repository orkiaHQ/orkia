// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! `orkia-check` — full-stack E2E gate binary.
//!
//! The flow registry is intentionally empty; F001..F005 land in Part D.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(unsafe_code)]

mod cli;
mod flows;
mod report;
mod runner;

use clap::Parser;

use crate::cli::Cli;

#[derive(Debug, thiserror::Error)]
enum CheckError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    match real_main().await {
        Ok(code) => std::process::ExitCode::from(code),
        Err(e) => {
            eprintln!("orkia-check: fatal: {e}");
            std::process::ExitCode::from(2)
        }
    }
}

async fn real_main() -> Result<u8, CheckError> {
    let cli = Cli::parse();

    if cli.list {
        print_list();
        return Ok(0);
    }

    let outcome = runner::run(&cli).await;
    emit(&cli, &outcome.report)?;
    Ok(u8::try_from(outcome.exit_code).unwrap_or(1))
}

fn print_list() {
    let reg = flows::registry();
    if reg.is_empty() {
        println!("(no flows registered yet — pending S0 Part D)");
    } else {
        for f in reg {
            println!("{}\t{}", f.id, f.name);
        }
    }
}

fn emit(cli: &Cli, report: &report::CheckReport) -> Result<(), CheckError> {
    if cli.json {
        let s = serde_json::to_string(report)?;
        println!("{s}");
    } else {
        emit_human(cli, report);
    }
    Ok(())
}

fn emit_human(cli: &Cli, report: &report::CheckReport) {
    println!(
        "orkia-check: status={:?} mode={:?} flows={} duration_ms={}",
        report.status, report.mode, report.summary.total, report.duration_ms,
    );
    println!(
        "  summary: passed={} failed={} skipped={} errored={}",
        report.summary.passed,
        report.summary.failed,
        report.summary.skipped,
        report.summary.errored,
    );
    if cli.verbose {
        for f in &report.flows {
            println!("  - {} [{:?}] {}ms", f.id, f.status, f.duration_ms);
        }
        for fail in &report.failures {
            println!("  ! {}: {}", fail.code, fail.message);
        }
    }
}

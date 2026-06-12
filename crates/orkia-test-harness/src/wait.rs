// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Generic polling helper.
//!
//! Most assertions in the harness are eventual — a hook envelope
//! arrives "soon", a screen update lands "soon". Hard-coded sleeps
//! flake. This helper polls a predicate at a fixed cadence and
//! returns either the produced value or a `WaitError` with a
//! diagnostic snapshot supplied by the caller.

use std::future::Future;
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum WaitError {
    #[error("timed out after {elapsed:?}: {context}")]
    Timeout { elapsed: Duration, context: String },
}

/// Poll `f` every `interval` until it returns `Some(v)` (returning
/// `Ok(v)`) or `timeout` elapses. On timeout, `context()` is called
/// once to produce a diagnostic string included in the error.
///
/// `f` is async because callers typically hold locks across `.await`
/// points (journal tail, PTY reader) and we want them to participate
/// in the same runtime.
pub async fn wait_for<F, Fut, T, C>(
    timeout: Duration,
    interval: Duration,
    mut f: F,
    context: C,
) -> Result<T, WaitError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
    C: FnOnce() -> String,
{
    let start = Instant::now();
    loop {
        if let Some(v) = f().await {
            return Ok(v);
        }
        if start.elapsed() >= timeout {
            return Err(WaitError::Timeout {
                elapsed: start.elapsed(),
                context: context(),
            });
        }
        tokio::time::sleep(interval).await;
    }
}

// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Error type for `orkia-stream`.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum StreamError {
    #[error("config error: {0}")]
    Config(String),

    #[error("backend URL: {0}")]
    BackendUrl(#[from] orkia_shell_types::backend::BackendUrlError),

    #[error("http client init: {0}")]
    HttpInit(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

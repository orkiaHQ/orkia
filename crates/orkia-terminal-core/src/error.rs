// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Engine error type. Variants are programmatic identifiers — never shown to
//! a user. The application crate maps them to localized messages.

use orkia_pty::PtyError;

#[derive(thiserror::Error, Debug)]
pub enum EngineError {
    #[error("pty: {0}")]
    Pty(#[from] PtyError),
}

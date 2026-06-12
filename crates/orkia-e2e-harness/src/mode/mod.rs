// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Execution modes for an [`crate::OrkiaSession`].

pub mod compose;
pub mod local;

/// Which backend a session is driving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// In-process backend (fast, dev iteration).
    Local,
    /// docker-compose stack (real images, used in CI).
    Compose,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Local => "local",
            Mode::Compose => "compose",
        }
    }
}

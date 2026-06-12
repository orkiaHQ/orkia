// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! End-to-end harness for the Orkia full-stack gate.
//!
//! API shape (`OrkiaSession`, four assertion builders, two modes) is
//! frozen; method bodies return [`HarnessError::NotImplemented`] until
//! the flow implementation lands in Part D.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(unsafe_code)]

pub mod assert;
pub mod boot;
pub mod env;
pub mod error;
pub mod fixtures;
pub mod login;
pub mod mode;
pub mod pty;
pub mod scripts;
pub mod session;

pub use assert::{BackendAssert, FileAssert, JournalAssert, OutputAssert};
pub use env::{FlowEnv, Plan};
pub use error::{AssertKind, HarnessError};
pub use mode::Mode;
pub use orkia_test_harness::{JournalEvent, OrkiaBinary};
pub use session::{OrkiaSession, RenderedOutput};

/// Crate-wide `Result` alias.
pub type Result<T> = std::result::Result<T, HarnessError>;

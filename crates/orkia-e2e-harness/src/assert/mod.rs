// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Assertion builders. Each consumes `self` and returns
//! `Result<(), HarnessError>` so flows chain naturally.

pub mod backend;
pub mod files;
pub mod journal;
pub mod output;

pub use backend::BackendAssert;
pub use files::FileAssert;
pub use journal::JournalAssert;
pub use output::OutputAssert;

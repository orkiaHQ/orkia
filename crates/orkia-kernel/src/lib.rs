// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.
#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! `orkia-kernel` — the Orkia Intelligence gate (client side).
//!
//! A single crate that owns and gates every intelligence feature: the login +
//! premium [`gate`], local model lifecycle ([`models`], delegating to the
//! kernel daemon), the classification wrapper ([`classify`]), and the
//! [`reasoning`] subsystem (hot-path consumer + enrich). The REPL boots one
//! [`Intelligence`] handle; when the gate is closed nothing is constructed.

pub mod classify;
pub mod gate;
pub mod intelligence;
pub mod models;
pub mod reasoning;

pub use classify::Classifier;
pub use gate::{Gate, GateState, Identity};
pub use intelligence::{BootConfig, Intelligence, KernelError};
pub use models::{ModelError, Models};
pub use reasoning::{
    BackfillSyncConfig, CaptureScope, JobScope, JobScopes, ReasoningAudit, ReasoningConsumer,
    backfill_sync, enrich_system_prompt, new_job_scopes, scope_for,
};

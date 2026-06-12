// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! RFC service façade.
//!
//! Composes [`orkia_rfc_core`] (persistence + state machine) with
//! [`orkia_rfc_lock`] (single-writer lock) and emits SEAL events through a
//! pluggable [`EventSink`] so the shell layer can route them to its existing
//! `event_router.on_custom()` pipeline.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod events;
pub mod service;

pub use events::{EventSink, RfcEvent};
pub use service::{AskRequest, EditRequest, LogDecisionRequest, RfcContext, RfcStateService};

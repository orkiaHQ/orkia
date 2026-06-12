// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Journal envelope types shared between the shell, the bridge, the
//! journal listener, and downstream consumers (e.g. `orkia-final-response`,
//! Team). Implementation logic — listener, store, normalisation — lives
//! in `orkia-shell`. This module owns only the data shape.

pub mod types;

pub use types::{EventType, JournalEnvelope, JournalFilter};

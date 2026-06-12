// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Error type for the journal hub.

/// Failure constructing or binding the journal hub. The shell layer
/// converts this into its own `ShellError` at the boundary; keeping a
/// dedicated type here avoids a dependency edge back into `orkia-shell`.
#[derive(Debug, thiserror::Error)]
pub enum JournalHubError {
    /// Socket directory creation, bind, or other listener setup failure.
    #[error("{0}")]
    Listener(String),
}

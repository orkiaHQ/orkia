// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

#[derive(Debug, Clone, PartialEq)]
pub enum IntentGuess {
    Command,
    Agent,
}

/// Decides whether a free-text line is meant as a shell command (sent to
/// brush) or as an agent intent (routed to an agent). Stateless — no
/// external command set is consulted; brush itself returns 127 if a
/// guessed-shell line names a missing binary, so an imperfect guess is
/// recoverable.
pub trait IntentClassifier: Send + Sync + 'static {
    fn classify(&self, line: &str) -> IntentGuess;
}

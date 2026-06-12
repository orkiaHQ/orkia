// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Autocomplete bridge between rustyline and brush.
//!
//! - [`CompletionProvider`] is the sync abstraction rustyline-side code
//!   talks to.
//! - [`brush_bridge::BrushCompletionProvider`] implements it by relaying
//!   requests to an async worker that owns the brush session.
//! - [`OrkiaHelper`] is the rustyline `Helper` impl: it merges brush
//!   candidates with orkia-specific ones (builtins, `@agent` names) and
//!   falls back to a local file/builtin completer when brush errors.

pub mod brush_bridge;
mod helper;
pub mod syntax;

pub use brush_bridge::BrushCompletionProvider;
pub use helper::{HelperShared, OrkiaHelper};

#[derive(Debug, Clone, Default)]
pub struct Suggestion {
    /// Byte offset into the input where the replacement should start.
    pub insertion_index: usize,
    /// Number of bytes to delete starting at `insertion_index` before
    /// inserting the candidate. Most providers leave this at 0 and
    /// expect rustyline to insert in-place at the cursor.
    pub replace_len: usize,
    pub candidates: Vec<String>,
}

impl Suggestion {
    pub fn empty() -> Self {
        Self::default()
    }
}

pub trait CompletionProvider: Send + Sync {
    fn complete(&self, line: &str, pos: usize) -> Suggestion;
}

/// Provider that always returns no candidates. Used when brush isn't
/// available yet (e.g. before `boot_brush_for_run` returns) so the
/// rustyline helper can still be constructed.
pub struct NullProvider;

impl CompletionProvider for NullProvider {
    fn complete(&self, _line: &str, _pos: usize) -> Suggestion {
        Suggestion::empty()
    }
}

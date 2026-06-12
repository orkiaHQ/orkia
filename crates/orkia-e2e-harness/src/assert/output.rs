// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Assertions over the last rendered output.

use crate::error::{AssertKind, HarnessError};
use crate::session::RenderedOutput;

pub struct OutputAssert<'a> {
    output: &'a RenderedOutput,
}

impl<'a> OutputAssert<'a> {
    pub fn new(output: &'a RenderedOutput) -> Self {
        Self { output }
    }

    pub fn contains(self, needle: &str) -> crate::Result<()> {
        if self.output.stripped.contains(needle) {
            return Ok(());
        }
        Err(HarnessError::assertion(
            format!("output.contains({needle:?}): not found"),
            AssertKind::Output,
            self.state_dump(),
        ))
    }

    pub fn has_line(self, line: &str) -> crate::Result<()> {
        if self.output.lines.iter().any(|l| l.trim() == line.trim()) {
            return Ok(());
        }
        Err(HarnessError::assertion(
            format!("output.has_line({line:?}): no matching line"),
            AssertKind::Output,
            self.state_dump(),
        ))
    }

    /// Pass if any of `needles` appears in the rendered screen.
    /// Useful when the implementation may have a few valid output
    /// shapes (e.g. "running" vs "background").
    pub fn contains_any(self, needles: &[&str]) -> crate::Result<()> {
        if needles.iter().any(|n| self.output.stripped.contains(n)) {
            return Ok(());
        }
        Err(HarnessError::assertion(
            format!("output.contains_any({needles:?}): none found"),
            AssertKind::Output,
            self.state_dump(),
        ))
    }

    pub fn not_contains(self, needle: &str) -> crate::Result<()> {
        if !self.output.stripped.contains(needle) {
            return Ok(());
        }
        Err(HarnessError::assertion(
            format!("output.not_contains({needle:?}): unexpectedly present"),
            AssertKind::Output,
            self.state_dump(),
        ))
    }

    /// Pre-formatted dump of the screen + truncated raw bytes for
    /// failure diagnostics. ANSI escapes are shown as `\e…` so JSON
    /// surfaces (`orkia-check --json`) stay readable.
    fn state_dump(&self) -> String {
        let raw_excerpt: String = self
            .output
            .raw
            .replace('\x1b', "\\e")
            .chars()
            .rev()
            .take(2000)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        format!(
            "--- rendered screen ({} bytes) ---\n{}\n--- raw tail (last 2000 chars, escapes shown as \\e) ---\n{}",
            self.output.stripped.len(),
            self.output.stripped,
            raw_excerpt
        )
    }
}

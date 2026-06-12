// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! TUI renderer for the Orkia shell.
//!
//! Implements [`orkia_shell_types::ShellRenderer`] on top of ratatui + crossterm
//! with a 3-region layout: sidebar (agents/jobs/projects) | main pane (scrollable
//! blocks) | input + status bar.
//!
//! Widget-mode attach (an embedded PTY pane that kept the sidebar
//! visible) is intentionally absent — see audit P3-001 and the
//! module-level docs on [`renderer::TuiRenderer`]. The REPL routes
//! foregrounding through the yield/reclaim path which uses a raw byte
//! splice (`orkia-shell::job::raw_attach`), the only correct way to
//! pipe a TUI agent through orkia today.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod app;
pub mod card;
pub mod daemon;
pub mod input;
pub mod layout;
pub mod renderer;
pub mod source_refs;
pub mod theme;
pub mod widgets;

pub use layout::{LayoutRects, ShellLayout};
pub use renderer::TuiRenderer;
pub use theme::Theme;

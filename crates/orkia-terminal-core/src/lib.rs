// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Orkia terminal engine: the validated three-thread lock-free snapshot model
//! (Reader / Extractor / Render). gpui-free and UI-string-free — it exposes
//! typed errors and `tracing` diagnostics only. See `ARCHITECTURE-TERMINAL.md`
//! for the threading contract and the tripwire list.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod ansi;
pub mod blocks;
pub mod config;
pub mod cursor;
pub mod engine;
pub mod error;
pub mod prescan;
pub mod render_snapshot;
pub mod screen_view;
pub mod state;
pub mod theme;
pub mod wake;

pub use blocks::{ApcCallback, Osc133Callback, Osc133Marker};
pub use config::EngineConfig;
pub use cursor::{CursorInfo, CursorShape, extract_cursor};
pub use engine::{AdoptMaster, RawOutputRx, TerminalEngine};
pub use error::EngineError;
pub use screen_view::{ScreenSnapshot, ScreenView};
pub use wake::{Wake, WakeRx, wake_pair};

// Re-export the PTY handle types the application binds to, so it depends on
// this crate as the single engine entry point.
pub use orkia_pty::{
    Dims, EventProxy, ScreenTerm, SharedDims, SharedMaster, SharedWriter, apply_resize,
};
